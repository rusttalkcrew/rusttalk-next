//! Reports (#678): a member reports a message they can read. The report (reason, note,
//! the sender's signed message and the key of only that message) is sealed on their
//! device to the devices of the people who review reports: members with
//! `review_reports`, or for a DM only the owner. The server stores ciphertext plus who
//! reported which message, and that message as stored (device, epoch, envelope hash), so
//! it can check the message exists, let reviewers check the evidence is that message,
//! download its files and act on it. Handling is audited; making a report is not, so the
//! audit log doesn't tell its readers who reported whom.

use std::collections::{BTreeSet, HashMap, HashSet};

use axum::{
    Json,
    extract::{
        Path, Query, State,
        rejection::{JsonRejection, QueryRejection},
    },
    http::{HeaderMap, StatusCode},
};
use rusttalk_core::{
    auth::{decode, decode_bytes, encode},
    identity::{REVOCATIONS_PER_PERSON, Revocation},
    keys::Event,
    reports::{
        MAX_REPORT_BYTES, MAX_REPORT_FILES, MAX_REVIEWER_DEVICES, NewReport, ReportRecord,
        Resolution, SignedRevocation, StoredMessage, WRAP_BYTES,
    },
};
use serde::{Deserialize, Serialize};
use serde_json::json;
use sqlx::{FromRow, SqliteConnection, query, query_as, query_scalar};

use crate::{
    ApiError, ApiResult, App,
    auth::{Actor, authenticate, authenticate_read},
    keys::conflict,
    limits, messages, payload,
    roles::{self, Permission},
    store::{self, audit_details},
    timeouts,
};

/// The smallest sealed report: nonce, key commitment, tag and signature.
const MIN_REPORT_BYTES: usize = 24 + 32 + 16 + 64;
/// Open reports one page lists, oldest first; `after` continues.
const PAGE: i64 = 100;
/// Open reports one member may have at once, and the whole server, each counted apart
/// for DMs and channels, so channel spam can't block DM reports. Every open report counts,
/// also a banned member's (which stay listed), so a queue never holds more than the
/// app's ten pages and storage stays bounded.
const OPEN_PER_REPORTER: i64 = 20;
const OPEN_PER_SERVER: i64 = 1000;

/// The people who review reports from `dm` or other channels: the owner, and for other
/// channels also members whose roles include `review_reports`.
async fn reviewers(db: &mut SqliteConnection, dm: bool) -> ApiResult<Vec<Vec<u8>>> {
    Ok(query_scalar(
        "SELECT owner_person_id FROM server_settings WHERE id = 1 AND owner_person_id IS NOT NULL
         UNION
         SELECT m.person_id FROM member_roles m JOIN roles r ON r.id = m.role_id
         WHERE NOT ? AND (r.permissions & ?) <> 0
           AND m.person_id IN (SELECT person_id FROM members)
         ORDER BY 1",
    )
    .bind(dm)
    .bind(Permission::ReviewReports as i64)
    .fetch_all(db)
    .await?)
}

/// Who a report of `sender`'s message goes to: the reviewers, but never the sender
/// unless they are the owner (who reviews everything, DMs too).
async fn recipients(db: &mut SqliteConnection, dm: bool, sender: &[u8]) -> ApiResult<Vec<Vec<u8>>> {
    let all = reviewers(&mut *db, dm).await?;
    if dm || is_owner(db, sender).await? {
        return Ok(all);
    }
    Ok(all.into_iter().filter(|person| person != sender).collect())
}

/// What the actor may review: reports from other channels, and (owner) from DMs.
struct Review {
    channels: bool,
    dms: bool,
}

async fn may_review(db: &mut SqliteConnection, person: &[u8]) -> ApiResult<Review> {
    let auth = roles::authority(&mut *db, person).await?;
    let owner = is_owner(db, person).await?;
    Ok(Review {
        channels: owner || auth.has(Permission::ReviewReports),
        dms: owner,
    })
}

async fn is_owner(db: &mut SqliteConnection, person: &[u8]) -> ApiResult<bool> {
    Ok(query_scalar(
        "SELECT EXISTS (SELECT 1 FROM server_settings WHERE id = 1 AND owner_person_id = ?)",
    )
    .bind(person)
    .fetch_one(db)
    .await?)
}

#[derive(Deserialize)]
pub struct ReviewersQuery {
    channel: String,
}

#[derive(Serialize)]
pub struct Reviewers {
    reviewers: Vec<String>,
}

/// Who reviews reports from this channel, so the reporter can show them and seal to their
/// devices. The reporter's app leaves out the reported sender unless they are the owner.
pub async fn list_reviewers(
    State(app): State<App>,
    headers: HeaderMap,
    query: Result<Query<ReviewersQuery>, QueryRejection>,
) -> ApiResult<Json<Reviewers>> {
    let Query(ReviewersQuery { channel }) =
        query.map_err(|_| ApiError::bad_request("invalid_query"))?;
    let (mut tx, actor) = authenticate_read(&app, &headers).await?;
    let kind = messages::chat_kind(&mut tx, &actor, &channel).await?;
    let reviewers = reviewers(&mut tx, kind == "dm").await?;
    tx.commit().await?;
    Ok(Json(Reviewers {
        reviewers: reviewers.iter().map(|person| encode(person)).collect(),
    }))
}

/// A message as stored: sender, signing device, epoch and envelope.
#[derive(FromRow)]
struct Stored {
    sender_person: Vec<u8>,
    sender_device: Vec<u8>,
    epoch: i64,
    envelope: Vec<u8>,
}

async fn stored(db: &mut SqliteConnection, channel: &str, id: &str) -> ApiResult<Option<Stored>> {
    Ok(query_as(
        "SELECT sender_person, sender_device, epoch, envelope FROM messages
         WHERE channel_id = ? AND id = ?",
    )
    .bind(channel)
    .bind(id)
    .fetch_optional(db)
    .await?)
}

/// Whether this record's nonce prefix was already shared by two stored records, so its
/// disclosed key would open another (see migration 0054).
async fn prefix_reused(
    db: &mut SqliteConnection,
    channel: &str,
    record: &Stored,
) -> ApiResult<bool> {
    let prefix = record.envelope.get(1..17).unwrap_or_default();
    Ok(query_scalar(
        "SELECT EXISTS (SELECT 1 FROM nonce_prefixes
           WHERE channel_id = ? AND epoch = ? AND prefix = ? AND reused)",
    )
    .bind(channel)
    .bind(record.epoch)
    .bind(prefix)
    .fetch_one(db)
    .await?)
}

/// The sender of a message in this channel.
async fn sender_of(
    db: &mut SqliteConnection,
    channel: &str,
    id: &str,
) -> ApiResult<Option<Vec<u8>>> {
    Ok(
        query_scalar("SELECT sender_person FROM messages WHERE channel_id = ? AND id = ?")
            .bind(channel)
            .bind(id)
            .fetch_optional(db)
            .await?,
    )
}

pub async fn create(
    State(app): State<App>,
    headers: HeaderMap,
    body: Result<Json<NewReport>, JsonRejection>,
) -> ApiResult<StatusCode> {
    let report = payload(body)?;
    let invalid = || ApiError::bad_request("invalid_report");
    for id in [&report.id, &report.channel_id, &report.message_id]
        .into_iter()
        .chain(&report.edit_id)
        .chain(&report.file_ids)
    {
        decode::<32>(id)?;
    }
    let sealed = decode_bytes(&report.body)?;
    if !(MIN_REPORT_BYTES..=MAX_REPORT_BYTES).contains(&sealed.len())
        || report.file_ids.len() > MAX_REPORT_FILES
        || report.keys.is_empty()
        || report.keys.len() > MAX_REVIEWER_DEVICES
        || report.omitted.len() > MAX_REVIEWER_DEVICES
        || report.edit_id.as_ref() == Some(&report.message_id)
    {
        return Err(invalid());
    }
    let keys = report
        .keys
        .iter()
        .map(|key| {
            let wrapped = decode_bytes(&key.key)?;
            if wrapped.len() != WRAP_BYTES {
                return Err(invalid());
            }
            Ok((decode::<32>(&key.device)?.to_vec(), wrapped))
        })
        .collect::<ApiResult<Vec<_>>>()?;
    // Signed removals are checked only for a session within its rate limit, and before
    // the write turn, so they neither cost an unknown caller nor hold up other writes.
    if !report.omitted.is_empty() {
        drop(authenticate_read(&app, &headers).await?);
    }
    let omitted = (report.omitted.iter())
        .map(|signed| {
            let person = decode::<32>(&signed.person)?;
            Revocation::verify(person, &decode_bytes(&signed.record)?)
                .map_err(|_| ApiError::bad_request("invalid_revocation"))
        })
        .collect::<ApiResult<Vec<_>>>()?;
    let (mut tx, actor) = authenticate(&app, &headers).await?;
    let channel = &report.channel_id;
    let dm = messages::chat_kind(&mut tx, &actor, channel).await? == "dm";
    let message = stored(&mut tx, channel, &report.message_id)
        .await?
        .ok_or(ApiError::not_found("message_not_found"))?;
    let sender = message.sender_person.clone();
    if sender == actor.person {
        return Err(ApiError::bad_request("cannot_report_own"));
    }
    // The shown edit is another message of the same person, in the same channel.
    let edit = match &report.edit_id {
        Some(edit) => match stored(&mut tx, channel, edit).await? {
            Some(edit) if edit.sender_person == sender => Some(edit),
            _ => return Err(invalid()),
        },
        None => None,
    };
    for record in std::iter::once(&message).chain(&edit) {
        if prefix_reused(&mut tx, channel, record).await? {
            return Err(conflict("nonce_reused"));
        }
    }
    let open: (i64, i64) = query_as(
        "SELECT COUNT(*) FILTER (WHERE reporter_id = ?), COUNT(*) FROM reports
         WHERE resolved_at IS NULL AND dm = ?",
    )
    .bind(actor.person.as_slice())
    .bind(dm)
    .fetch_one(&mut *tx)
    .await?;
    if open.0 >= OPEN_PER_REPORTER {
        return Err(conflict("too_many_reports"));
    }
    if open.1 >= OPEN_PER_SERVER {
        return Err(conflict("report_queue_full"));
    }
    // The wraps go to exactly the current devices of the recipients, each once: none
    // added, and none left out unless the reporter shows that its owner removed it.
    let recipients = recipients(&mut tx, dm, &sender).await?;
    let mut devices = HashSet::new();
    for person in &recipients {
        let theirs: Vec<Vec<u8>> =
            query_scalar("SELECT device_id FROM member_devices WHERE person_id = ?")
                .bind(person)
                .fetch_all(&mut *tx)
                .await?;
        devices.extend(theirs);
    }
    for removal in &omitted {
        let listed: Option<Vec<u8>> =
            query_scalar("SELECT person_id FROM member_devices WHERE device_id = ?")
                .bind(removal.device_signing_key().as_slice())
                .fetch_optional(&mut *tx)
                .await?;
        // A device already gone here (its removal arrived meanwhile) is simply not owed
        // a wrap; one listed for someone else is a forgery.
        match listed {
            None => {}
            Some(person) if person.as_slice() == removal.person_id() => {
                devices.remove(removal.device_signing_key().as_slice());
            }
            Some(_) => return Err(ApiError::bad_request("invalid_revocation")),
        }
    }
    if devices.len() > MAX_REVIEWER_DEVICES {
        return Err(conflict("too_many_reviewers"));
    }
    let given: HashSet<&Vec<u8>> = keys.iter().map(|(device, _)| device).collect();
    if given.len() != keys.len() || given != devices.iter().collect() {
        return Err(conflict("reviewers_changed"));
    }
    // Only the sender's own files of this channel that are still kept: a report opens
    // nothing else, and a file that is gone doesn't stop the report.
    let mut files = BTreeSet::new();
    for file in report.file_ids.iter().collect::<BTreeSet<_>>() {
        let kept: bool = query_scalar(
            "SELECT EXISTS (SELECT 1 FROM files WHERE id = ? AND channel_id = ? AND uploader = ?
               AND expires_at > ?)",
        )
        .bind(file)
        .bind(channel)
        .bind(&sender)
        .bind(actor.at)
        .fetch_one(&mut *tx)
        .await?;
        if kept {
            files.insert(file);
        }
    }
    // Charged only for a report that is otherwise accepted, so a stale reviewer list
    // retried doesn't use up the allowance.
    app.limits.check(&actor, limits::Action::Report)?;
    let inserted = query(
        "INSERT INTO reports (id, reporter_id, channel_id, message_id, message_device,
           message_epoch, message_hash, edit_id, edit_device, edit_epoch, edit_hash, sender_id,
           dm, body_ct, created_at)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
         ON CONFLICT (reporter_id, message_id) WHERE resolved_at IS NULL DO NOTHING",
    )
    .bind(&report.id)
    .bind(actor.person.as_slice())
    .bind(channel)
    .bind(&report.message_id)
    .bind(&message.sender_device)
    .bind(message.epoch)
    .bind(store::hash(&message.envelope).as_slice())
    .bind(&report.edit_id)
    .bind(edit.as_ref().map(|edit| edit.sender_device.clone()))
    .bind(edit.as_ref().map(|edit| edit.epoch))
    .bind(
        edit.as_ref()
            .map(|edit| store::hash(&edit.envelope).to_vec()),
    )
    .bind(&sender)
    .bind(dm)
    .bind(&sealed)
    .bind(actor.at)
    .execute(&mut *tx)
    .await
    .map_err(|error| match error.as_database_error() {
        Some(db) if db.is_unique_violation() => conflict("report_exists"),
        _ => error.into(),
    })?;
    if inserted.rows_affected() == 0 {
        return Err(conflict("already_reported"));
    }
    for (device, wrapped) in &keys {
        query("INSERT INTO report_keys (report_id, device_id, wrapped) VALUES (?, ?, ?)")
            .bind(&report.id)
            .bind(device)
            .bind(wrapped)
            .execute(&mut *tx)
            .await?;
    }
    for file in files {
        query("INSERT INTO report_files (report_id, file_id) VALUES (?, ?)")
            .bind(&report.id)
            .bind(file)
            .execute(&mut *tx)
            .await?;
    }
    tx.commit().await?;
    // Who reported is not logged; the report ID ties the log to the row.
    tracing::info!(report = report.id, dm, "message reported");
    Ok(StatusCode::CREATED)
}

#[derive(FromRow)]
struct Row {
    id: String,
    channel_id: String,
    message_id: String,
    message_device: Vec<u8>,
    message_epoch: i64,
    message_hash: Vec<u8>,
    edit_id: Option<String>,
    edit_device: Option<Vec<u8>>,
    edit_epoch: Option<i64>,
    edit_hash: Option<Vec<u8>>,
    reporter_id: Vec<u8>,
    sender_id: Vec<u8>,
    dm: bool,
    created_at: i64,
    body_ct: Vec<u8>,
    wrapped: Option<Vec<u8>>,
}

#[derive(Deserialize)]
pub struct Page {
    /// Continue after this report: its time and ID, as the last one listed.
    after_at: Option<i64>,
    after_id: Option<String>,
    /// Only DM reports, or only channel reports, so a full queue of one hides none of the
    /// other.
    dm: Option<bool>,
}

/// Open reports the actor may review, oldest first, a page at a time, each with the
/// report key wrapped to the asking device and the removals of its sender's and
/// reporter's devices. Nobody but the owner sees reports about themselves. Reports by
/// banned members stay until handled or dismissed: a ban never hides a report, so a
/// reported moderator can't make one go away by banning its reporter.
pub async fn list(
    State(app): State<App>,
    headers: HeaderMap,
    page: Result<Query<Page>, QueryRejection>,
) -> ApiResult<Json<Vec<ReportRecord>>> {
    let Query(Page {
        after_at,
        after_id,
        dm,
    }) = page.map_err(|_| ApiError::bad_request("invalid_query"))?;
    let (mut tx, actor) = authenticate_read(&app, &headers).await?;
    let review = may_review(&mut tx, &actor.person).await?;
    if !review.channels && !review.dms {
        return Err(ApiError::forbidden("permission_denied"));
    }
    let rows: Vec<Row> = query_as(
        "SELECT r.id, r.channel_id, r.message_id, r.message_device, r.message_epoch,
           r.message_hash, r.edit_id, r.edit_device, r.edit_epoch, r.edit_hash, r.reporter_id,
           r.sender_id, r.dm, r.created_at, r.body_ct, k.wrapped
         FROM reports r LEFT JOIN report_keys k ON k.report_id = r.id AND k.device_id = ?
         WHERE r.resolved_at IS NULL AND ((r.dm AND ?) OR (NOT r.dm AND ?))
           AND (r.sender_id <> ? OR ?)
           AND (? IS NULL OR r.dm = ?)
           AND (? IS NULL OR (r.created_at, r.id) > (?, ?))
         ORDER BY r.created_at, r.id LIMIT ?",
    )
    .bind(actor.device.as_slice())
    .bind(review.dms)
    .bind(review.channels)
    .bind(actor.person.as_slice())
    .bind(review.dms)
    .bind(dm)
    .bind(dm)
    .bind(after_at)
    .bind(after_at)
    .bind(after_id.unwrap_or_default())
    .bind(PAGE)
    .fetch_all(&mut *tx)
    .await?;
    // The device removals of everyone the page names, in one query.
    let people: BTreeSet<&[u8]> = (rows.iter())
        .flat_map(|row| [row.sender_id.as_slice(), row.reporter_id.as_slice()])
        .collect();
    let mut removals: HashMap<Vec<u8>, Vec<Vec<u8>>> = HashMap::new();
    if !people.is_empty() {
        let marks = vec!["?"; people.len()].join(",");
        let sql = format!(
            "SELECT person_id, record FROM revocations WHERE person_id IN ({marks})
             ORDER BY person_id, device_id"
        );
        let mut select = query_as::<_, (Vec<u8>, Vec<u8>)>(&sql);
        for person in &people {
            select = select.bind(*person);
        }
        for (person, record) in select.fetch_all(&mut *tx).await? {
            let theirs = removals.entry(person).or_default();
            if theirs.len() < REVOCATIONS_PER_PERSON {
                theirs.push(record);
            }
        }
    }
    let mut list = Vec::with_capacity(rows.len());
    for row in rows {
        let mut people = vec![&row.sender_id];
        if row.reporter_id != row.sender_id {
            people.push(&row.reporter_id);
        }
        let revocations: Vec<SignedRevocation> = (people.into_iter())
            .flat_map(|person| {
                (removals.get(person).into_iter().flatten()).map(|record| SignedRevocation {
                    person: encode(person),
                    record: encode(record),
                })
            })
            .collect();
        let mut stored = vec![StoredMessage {
            id: row.message_id.clone(),
            device: encode(&row.message_device),
            epoch: row.message_epoch,
            hash: encode(&row.message_hash),
        }];
        if let (Some(id), Some(device), Some(epoch), Some(hash)) =
            (&row.edit_id, row.edit_device, row.edit_epoch, row.edit_hash)
        {
            stored.push(StoredMessage {
                id: id.clone(),
                device: encode(&device),
                epoch,
                hash: encode(&hash),
            });
        }
        list.push(ReportRecord {
            id: row.id,
            channel_id: row.channel_id,
            message_id: row.message_id,
            edit_id: row.edit_id,
            reporter: encode(&row.reporter_id),
            sender: encode(&row.sender_id),
            dm: row.dm,
            at: row.created_at,
            body: encode(&row.body_ct),
            key: row.wrapped.map(|key| encode(&key)),
            stored,
            revocations,
        });
    }
    tx.commit().await?;
    Ok(Json(list))
}

#[derive(Deserialize)]
pub struct Resolve {
    action: Resolution,
    /// With `deleted`: the stored files of the reported message, as the reviewer's app
    /// read them from the verified message. Only those this report covers are deleted.
    #[serde(default)]
    files: Vec<String>,
}

#[derive(FromRow)]
struct Open {
    channel_id: String,
    message_id: String,
    edit_id: Option<String>,
    sender_id: Vec<u8>,
    dm: bool,
    resolved_at: Option<i64>,
}

/// Close a report with what was done about it. A timeout, kick or ban is done first
/// through its own endpoint (which checks rank); this checks it took effect. Deleting
/// removes the reported message, the edit this report showed and the message's files,
/// which the reviewer verified, and nothing named only by other reports, so a reviewer
/// who can't read the channel can still remove it. Dismissing closes this report alone,
/// so dismissing a bogus report never drops another's evidence; an action closes the
/// other reports of the same message too, except, after a delete, those showing an edit
/// that is still there. Nobody but the owner handles a report about themselves.
pub async fn resolve(
    State(app): State<App>,
    headers: HeaderMap,
    Path(id): Path<String>,
    body: Result<Json<Resolve>, JsonRejection>,
) -> ApiResult<StatusCode> {
    let Resolve { action, files } = payload(body)?;
    decode::<32>(&id)?;
    if files.len() > MAX_REPORT_FILES {
        return Err(ApiError::bad_request("invalid_report"));
    }
    for file in &files {
        decode::<32>(file)?;
    }
    let (mut tx, actor) = authenticate(&app, &headers).await?;
    let report: Open = query_as(
        "SELECT channel_id, message_id, edit_id, sender_id, dm, resolved_at FROM reports
         WHERE id = ?",
    )
    .bind(&id)
    .fetch_optional(&mut *tx)
    .await?
    .ok_or(ApiError::not_found("report_not_found"))?;
    let review = may_review(&mut tx, &actor.person).await?;
    let allowed = if report.dm {
        review.dms
    } else {
        review.channels
    };
    if !allowed || (report.sender_id == actor.person && !review.dms) {
        return Err(ApiError::not_found("report_not_found"));
    }
    if report.resolved_at.is_some() {
        return Err(conflict("report_resolved"));
    }
    let mut deleted = Vec::new();
    let mut files_deleted = 0;
    let sender = report.sender_id.as_slice();
    let channel = &report.channel_id;
    let mut which = Which::Message(&report.message_id, channel, None);
    let taken = match action {
        Resolution::Dismissed => {
            which = Which::One(&id);
            true
        }
        Resolution::Deleted => {
            // DMs are private to their two people: the owner reviews, but doesn't delete.
            if report.dm {
                return Err(ApiError::forbidden("permission_denied"));
            }
            roles::authority(&mut tx, &actor.person)
                .await?
                .require(Permission::DeleteMessages)?;
            for message in std::iter::once(&report.message_id).chain(&report.edit_id) {
                if sender_of(&mut tx, channel, message).await?.as_deref() == Some(sender) {
                    // Not a DM (refused above), which is all `remove` needs to know.
                    messages::remove(&app, &mut tx, &actor, channel, message, "text").await?;
                    deleted.push(message.clone());
                }
            }
            // Its files too, as named by the reviewer and by this report, and still the
            // sender's in this channel.
            for file in files.iter().collect::<BTreeSet<_>>() {
                let covered: bool = query_scalar(
                    "SELECT EXISTS (SELECT 1 FROM report_files rf JOIN files f ON f.id = rf.file_id
                       WHERE rf.report_id = ? AND rf.file_id = ? AND f.channel_id = ?
                         AND f.uploader = ?)",
                )
                .bind(&id)
                .bind(file)
                .bind(channel)
                .bind(sender)
                .fetch_one(&mut *tx)
                .await?;
                if covered {
                    app.files.remove(&mut tx, file).await?;
                    files_deleted += 1;
                }
            }
            which = Which::Message(&report.message_id, channel, Some(report.edit_id.as_deref()));
            true
        }
        // Only someone who may take the action closes a report with it.
        Resolution::TimedOut => {
            let auth = roles::authority(&mut tx, &actor.person).await?;
            auth.require(Permission::TimeoutMembers)?;
            timeouts::ends_at(&mut tx, sender, actor.at)
                .await?
                .is_some()
        }
        Resolution::Kicked => {
            let auth = roles::authority(&mut tx, &actor.person).await?;
            auth.require(Permission::Kick)?;
            !query_scalar::<_, bool>("SELECT EXISTS (SELECT 1 FROM members WHERE person_id = ?)")
                .bind(sender)
                .fetch_one(&mut *tx)
                .await?
        }
        Resolution::Banned => {
            let auth = roles::authority(&mut tx, &actor.person).await?;
            auth.require(Permission::Ban)?;
            query_scalar::<_, bool>("SELECT EXISTS (SELECT 1 FROM bans WHERE person_id = ?)")
                .bind(sender)
                .fetch_one(&mut *tx)
                .await?
        }
    };
    if !taken {
        return Err(conflict("action_not_taken"));
    }
    let resolution = serde_json::to_value(action)
        .ok()
        .and_then(|value| value.as_str().map(str::to_owned))
        .unwrap_or_default();
    let closed = close(&mut tx, &actor, which, &resolution).await?;
    // A DM report is the owner's alone: its entry is shown to the owner only (see
    // `members::audit_log`).
    let entry = match report.dm {
        true => "dm_report_resolved",
        false => "report_resolved",
    };
    audit_details(
        &mut tx,
        &actor.person,
        entry,
        Some(sender),
        Some(&id),
        Some(json!({ "resolution": resolution, "reports": closed, "files": files_deleted })),
        actor.at,
    )
    .await?;
    tx.commit().await?;
    for message in deleted {
        app.notify(Event::MessageDeleted {
            channel: report.channel_id.clone(),
            id: message,
        });
    }
    Ok(StatusCode::NO_CONTENT)
}

/// Which open reports to close.
enum Which<'a> {
    /// This one report.
    One(&'a str),
    /// Every report of this message (ID, channel); with `Some(edit)`, only those showing
    /// the original or that edit (`None`: the original), the ones a delete removed.
    Message(&'a str, &'a str, Option<Option<&'a str>>),
    /// Every channel report (`false`) or DM report (`true`) by this member, except those
    /// about the actor unless `true` (the owner, who handles reports about themselves).
    Reporter(&'a [u8], bool, bool),
}

/// Close open reports, dropping their evidence: only the row stays, for the log and
/// duplicates.
async fn close(
    db: &mut SqliteConnection,
    actor: &Actor,
    which: Which<'_>,
    resolution: &str,
) -> ApiResult<usize> {
    const SET: &str = "UPDATE reports SET resolved_at = ?, resolved_by = ?, resolution = ?,
        body_ct = X'' WHERE resolved_at IS NULL AND ";
    let update = |filter: &str| format!("{SET}{filter} RETURNING id");
    let closed: Vec<String> = match which {
        Which::One(id) => {
            query_scalar(&update("id = ?"))
                .bind(actor.at)
                .bind(actor.person.as_slice())
                .bind(resolution)
                .bind(id)
                .fetch_all(&mut *db)
                .await?
        }
        Which::Message(message, channel, edit) => {
            query_scalar(&update(
                "message_id = ? AND channel_id = ?
                 AND (? OR edit_id IS NULL OR edit_id IS ?)",
            ))
            .bind(actor.at)
            .bind(actor.person.as_slice())
            .bind(resolution)
            .bind(message)
            .bind(channel)
            .bind(edit.is_none())
            .bind(edit.flatten())
            .fetch_all(&mut *db)
            .await?
        }
        Which::Reporter(reporter, dm, own) => {
            query_scalar(&update(
                "dm = ? AND reporter_id = ? AND (sender_id <> ? OR ?)",
            ))
            .bind(actor.at)
            .bind(actor.person.as_slice())
            .bind(resolution)
            .bind(dm)
            .bind(reporter)
            .bind(actor.person.as_slice())
            .bind(own)
            .fetch_all(&mut *db)
            .await?
        }
    };
    for closed in &closed {
        query("DELETE FROM report_keys WHERE report_id = ?")
            .bind(closed)
            .execute(&mut *db)
            .await?;
        query("DELETE FROM report_files WHERE report_id = ?")
            .bind(closed)
            .execute(&mut *db)
            .await?;
    }
    Ok(closed.len())
}

#[derive(Deserialize)]
pub struct DismissKind {
    /// DM reports instead of channel reports: the owner's.
    #[serde(default)]
    dm: bool,
}

#[derive(Serialize)]
pub struct Dismissed {
    reports: usize,
}

/// Dismiss every open channel report by one member at once, such as a banned or removed
/// spammer's; with `dm=true`, the owner dismisses their DM reports instead. Reports about
/// the actor are left alone, unless the actor is the owner, who handles those anyway (as
/// in `list` and `resolve`), so a raid on the owner's messages can be cleared too.
/// Answers how many it closed.
pub async fn dismiss_from(
    State(app): State<App>,
    headers: HeaderMap,
    Path(person): Path<String>,
    kind: Result<Query<DismissKind>, QueryRejection>,
) -> ApiResult<Json<Dismissed>> {
    let Query(DismissKind { dm }) = kind.map_err(|_| ApiError::bad_request("invalid_query"))?;
    let reporter = decode::<32>(&person)?;
    let (mut tx, actor) = authenticate(&app, &headers).await?;
    let review = may_review(&mut tx, &actor.person).await?;
    if !(if dm { review.dms } else { review.channels }) {
        return Err(ApiError::forbidden("permission_denied"));
    }
    let which = Which::Reporter(&reporter, dm, review.dms);
    let closed = close(&mut tx, &actor, which, "dismissed").await?;
    if closed > 0 {
        // A DM entry is the owner's alone (see `members::audit_log`).
        audit_details(
            &mut tx,
            &actor.person,
            if dm {
                "dm_reports_dismissed"
            } else {
                "reports_dismissed"
            },
            Some(&reporter),
            None,
            Some(json!({ "reports": closed })),
            actor.at,
        )
        .await?;
    }
    tx.commit().await?;
    Ok(Json(Dismissed { reports: closed }))
}

/// Whether this device of the actor may download this stored file without reading its
/// channel: it was sealed an open report naming the file, which the actor still reviews
/// and which isn't about the actor (unless the owner).
pub async fn may_download(db: &mut SqliteConnection, actor: &Actor, file: &str) -> ApiResult<bool> {
    let review = may_review(&mut *db, &actor.person).await?;
    Ok(query_scalar(
        "SELECT EXISTS (SELECT 1 FROM report_files f JOIN reports r ON r.id = f.report_id
           JOIN report_keys k ON k.report_id = r.id AND k.device_id = ?
           WHERE f.file_id = ? AND r.resolved_at IS NULL AND ((r.dm AND ?) OR (NOT r.dm AND ?))
             AND (r.sender_id <> ? OR ?))",
    )
    .bind(actor.device.as_slice())
    .bind(file)
    .bind(review.dms)
    .bind(review.channels)
    .bind(actor.person.as_slice())
    .bind(review.dms)
    .fetch_one(db)
    .await?)
}

#[cfg(test)]
mod tests;
