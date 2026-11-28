use anyhow::Result;
use rusttalk_core::reports::ReportKey;

use std::collections::HashSet;

use super::*;
use crate::limits::Limits;
use crate::testing::{app, as_person, person};

const OWNER: u8 = 1;
const REPORTER: u8 = 2;
const SENDER: u8 = 3;
const MODERATOR: u8 = 4;
/// A member who can't read the channel.
const OUTSIDER: u8 = 5;
/// Both reviewers of channel reports, whose devices every channel report must name.
const CHANNEL_REVIEWERS: [u8; 2] = [OWNER, MODERATOR];

fn id(byte: u8) -> String {
    encode(&[byte; 32])
}

fn device(who: u8) -> Vec<u8> {
    vec![who + 100; 32]
}

/// A channel members read and a DM between the reporter and the sender, each with
/// messages, and files. Opaque rows, so no signatures are needed.
async fn seeded(dir: &std::path::Path) -> Result<App> {
    let app = app(dir, 5).await?;
    let (channel, dm) = (id(10), id(11));
    sqlx::raw_sql(&format!(
        "INSERT INTO categories VALUES ('category', 0, X'00');
         INSERT INTO channels (id, kind, category_id, position, name_ct)
             VALUES ('{channel}', 'text', 'category', 0, X'00'),
                    ('{other}', 'text', 'category', 1, X'00');
         INSERT INTO channel_permissions (channel_id, role_id, flags)
             VALUES ('{channel}', 'member', 3), ('{channel}', 'moderator', 3);",
        other = id(12),
    ))
    .execute(&app.db)
    .await?;
    query(
        "INSERT INTO channels (id, kind, position, dm_first, dm_second) VALUES (?, 'dm', 0, ?, ?)",
    )
    .bind(&dm)
    .bind(person(REPORTER))
    .bind(person(SENDER))
    .execute(&app.db)
    .await?;
    sqlx::raw_sql(
        "INSERT INTO key_scopes (id, kind, channel_id) SELECT id, 'channel', id FROM channels;
         INSERT INTO key_epochs (scope_id, epoch, commitment) SELECT id, 1, zeroblob(32) FROM key_scopes;",
    )
    .execute(&app.db)
    .await?;
    for (who, role) in [
        (REPORTER, "member"),
        (SENDER, "member"),
        (MODERATOR, "moderator"),
    ] {
        query("INSERT INTO member_roles (person_id, role_id) VALUES (?, ?)")
            .bind(person(who))
            .bind(role)
            .execute(&app.db)
            .await?;
    }
    for who in [OWNER, REPORTER, SENDER, MODERATOR] {
        // Real device IDs are Ed25519 keys; the sessions of `testing::app` use one byte.
        query("UPDATE sessions SET device_id = ? WHERE person_id = ?")
            .bind(device(who))
            .bind(person(who))
            .execute(&app.db)
            .await?;
        query(
            "INSERT INTO member_devices (device_id, person_id, certificate) VALUES (?, ?, X'00')",
        )
        .bind(device(who))
        .bind(person(who))
        .execute(&app.db)
        .await?;
        query("INSERT INTO message_senders VALUES (?, ?, X'03')")
            .bind(device(who))
            .bind(person(who))
            .execute(&app.db)
            .await?;
    }
    // 20 is the sender's, 21 and 25 edits of it, 22 the reporter's own, 23 in the DM and
    // 24 the moderator's.
    for (message, channel, who) in [
        (20, &channel, SENDER),
        (21, &channel, SENDER),
        (22, &channel, REPORTER),
        (23, &dm, SENDER),
        (24, &channel, MODERATOR),
        (25, &channel, SENDER),
    ] {
        query(
            "INSERT INTO messages (id, channel_id, epoch, sender_device, sender_person, at, size, envelope)
             VALUES (?, ?, 1, ?, ?, 0, 111, zeroblob(111))",
        )
        .bind(id(message))
        .bind(channel)
        .bind(device(who))
        .bind(person(who))
        .execute(&app.db)
        .await?;
    }
    query("UPDATE channels SET message_count = 5 WHERE id = ?")
        .bind(&channel)
        .execute(&app.db)
        .await?;
    // 30 is the sender's in the channel; 31 in another channel, 32 someone else's and 33
    // expired.
    for (file, channel, who, expires_at) in [
        (30, &channel, SENDER, 4_000_000_000i64),
        (31, &id(12), SENDER, 4_000_000_000),
        (32, &channel, REPORTER, 4_000_000_000),
        (33, &channel, SENDER, 1),
        (34, &channel, MODERATOR, 4_000_000_000),
    ] {
        query(
            "INSERT INTO files (id, channel_id, uploader, size, created_at, expires_at)
             VALUES (?, ?, ?, 16, 0, ?)",
        )
        .bind(id(file))
        .bind(channel)
        .bind(person(who))
        .bind(expires_at)
        .execute(&app.db)
        .await?;
    }
    Ok(app)
}

fn report(report: u8, channel: u8, message: u8, devices: &[u8]) -> NewReport {
    NewReport {
        id: id(report),
        channel_id: id(channel),
        message_id: id(message),
        edit_id: None,
        file_ids: Vec::new(),
        body: encode(&[0; 200]),
        omitted: Vec::new(),
        keys: devices
            .iter()
            .map(|who| ReportKey {
                device: encode(&device(*who)),
                key: encode(&[*who; WRAP_BYTES]),
            })
            .collect(),
    }
}

async fn send(app: &App, who: u8, new: NewReport) -> Result<StatusCode, &'static str> {
    create(State(app.clone()), as_person(who), Ok(Json(new)))
        .await
        .map_err(|error| error.code)
}

async fn reviewers_of(app: &App, channel: u8) -> Vec<String> {
    let query = Query(ReviewersQuery {
        channel: id(channel),
    });
    list_reviewers(State(app.clone()), as_person(REPORTER), Ok(query))
        .await
        .unwrap()
        .0
        .reviewers
}

async fn listed(app: &App, who: u8) -> Result<Vec<ReportRecord>, &'static str> {
    page(app, who, None, None).await
}

async fn page(
    app: &App,
    who: u8,
    after: Option<(i64, String)>,
    dm: Option<bool>,
) -> Result<Vec<ReportRecord>, &'static str> {
    let (after_at, after_id) = after.unzip();
    let query = Query(Page {
        after_at,
        after_id,
        dm,
    });
    list(State(app.clone()), as_person(who), Ok(query))
        .await
        .map(|Json(list)| list)
        .map_err(|error| error.code)
}

async fn settle(app: &App, who: u8, report: u8, action: Resolution) -> Result<(), &'static str> {
    settle_with(app, who, report, action, Vec::new()).await
}

async fn settle_with(
    app: &App,
    who: u8,
    report: u8,
    action: Resolution,
    files: Vec<String>,
) -> Result<(), &'static str> {
    resolve(
        State(app.clone()),
        as_person(who),
        Path(id(report)),
        Ok(Json(Resolve { action, files })),
    )
    .await
    .map(|_| ())
    .map_err(|error| error.code)
}

fn actor(who: u8) -> Actor {
    Actor {
        person: [who; 32],
        device: device(who),
        ip: String::new(),
        at: 0,
        session_id: Vec::new(),
        expires_at: 0,
    }
}

async fn report_files(app: &App, report: u8) -> Result<Vec<String>> {
    Ok(
        query_scalar("SELECT file_id FROM report_files WHERE report_id = ? ORDER BY file_id")
            .bind(id(report))
            .fetch_all(&app.db)
            .await?,
    )
}

#[tokio::test]
async fn reports_go_sealed_to_every_reviewer_device_exactly() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let app = seeded(dir.path()).await?;
    // The owner and moderators review channel reports; only the owner DM reports.
    assert_eq!(
        reviewers_of(&app, 10).await,
        [encode(&person(OWNER)), encode(&person(MODERATOR))]
    );
    assert_eq!(reviewers_of(&app, 11).await, [encode(&person(OWNER))]);

    // Each current reviewer device exactly once: none left out (M5), none added, no twice.
    for devices in [
        &[OWNER][..],
        &[OWNER, MODERATOR, REPORTER],
        &[OWNER, MODERATOR, MODERATOR],
    ] {
        assert_eq!(
            send(&app, REPORTER, report(1, 10, 20, devices)).await,
            Err("reviewers_changed"),
            "{devices:?}"
        );
    }
    assert_eq!(
        send(&app, REPORTER, report(1, 11, 23, &CHANNEL_REVIEWERS)).await,
        Err("reviewers_changed"),
        "a DM report is for the owner alone"
    );
    // Only messages the reporter can read, not their own.
    assert_eq!(
        send(&app, OUTSIDER, report(1, 10, 20, &CHANNEL_REVIEWERS)).await,
        Err("channel_read_denied")
    );
    assert_eq!(
        send(&app, REPORTER, report(1, 10, 22, &CHANNEL_REVIEWERS)).await,
        Err("cannot_report_own")
    );
    assert_eq!(
        send(&app, REPORTER, report(1, 10, 99, &CHANNEL_REVIEWERS)).await,
        Err("message_not_found")
    );
    // An edit by someone else, or the message as its own edit, is refused.
    for edit in [22, 20] {
        let mut wrong_edit = report(1, 10, 20, &CHANNEL_REVIEWERS);
        wrong_edit.edit_id = Some(id(edit));
        assert_eq!(
            send(&app, REPORTER, wrong_edit).await,
            Err("invalid_report")
        );
    }
    let mut short = report(1, 10, 20, &CHANNEL_REVIEWERS);
    short.keys[0].key = encode(&[1; 79]);
    assert_eq!(send(&app, REPORTER, short).await, Err("invalid_report"));

    let mut good = report(1, 10, 20, &CHANNEL_REVIEWERS);
    good.edit_id = Some(id(21));
    assert_eq!(send(&app, REPORTER, good).await, Ok(StatusCode::CREATED));
    assert_eq!(
        send(&app, REPORTER, report(2, 10, 20, &CHANNEL_REVIEWERS)).await,
        Err("already_reported")
    );
    assert_eq!(
        send(&app, REPORTER, report(3, 11, 23, &[OWNER])).await,
        Ok(StatusCode::CREATED)
    );

    // Members don't see reports; moderators see channel reports with their own key and
    // each message as stored (H1).
    assert_eq!(
        listed(&app, REPORTER).await.err(),
        Some("permission_denied")
    );
    let seen = listed(&app, MODERATOR).await.unwrap();
    assert_eq!(seen.len(), 1);
    let first = &seen[0];
    assert_eq!(first.id, id(1));
    assert_eq!(first.edit_id, Some(id(21)));
    assert_eq!(first.reporter, encode(&person(REPORTER)));
    assert_eq!(first.sender, encode(&person(SENDER)));
    assert_eq!(first.key, Some(encode(&[MODERATOR; WRAP_BYTES])));
    assert_eq!(first.body, encode(&[0; 200]));
    let hash = encode(&store::hash(&[0; 111]));
    let stored = |message: u8| StoredMessage {
        id: id(message),
        device: encode(&device(SENDER)),
        epoch: 1,
        hash: hash.clone(),
    };
    assert_eq!(first.stored, [stored(20), stored(21)]);
    let owner = listed(&app, OWNER).await.unwrap();
    assert_eq!(owner.len(), 2);
    assert!(owner[1].dm && owner[1].key == Some(encode(&[OWNER; WRAP_BYTES])));
    assert_eq!(owner[1].stored, [stored(23)]);
    Ok(())
}

#[tokio::test]
async fn a_report_opens_only_the_senders_kept_files_in_its_channel() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let app = seeded(dir.path()).await?;
    // Files elsewhere, someone else's or expired don't fail the report (H2) and aren't
    // opened by it (H3).
    let mut new = report(1, 10, 20, &CHANNEL_REVIEWERS);
    new.file_ids = [30, 30, 31, 32, 33].map(id).to_vec();
    assert_eq!(send(&app, REPORTER, new).await, Ok(StatusCode::CREATED));
    assert_eq!(report_files(&app, 1).await?, [id(30)]);

    // Reviewers download it through a device the report was sealed to; nobody else does.
    let mut db = app.db.acquire().await?;
    assert!(may_download(&mut db, &actor(MODERATOR), &id(30)).await?);
    assert!(!may_download(&mut db, &actor(REPORTER), &id(30)).await?);
    assert!(!may_download(&mut db, &actor(MODERATOR), &id(32)).await?);
    let other_device = Actor {
        device: vec![9; 32],
        ..actor(MODERATOR)
    };
    assert!(!may_download(&mut db, &other_device, &id(30)).await?);
    Ok(())
}

#[tokio::test]
async fn moderators_never_see_or_handle_reports_about_themselves() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let app = seeded(dir.path()).await?;
    // The reported moderator gets no key (M3): their device must be left out.
    let with_them = report(1, 10, 24, &CHANNEL_REVIEWERS);
    assert_eq!(
        send(&app, REPORTER, with_them).await,
        Err("reviewers_changed")
    );
    let mut new = report(1, 10, 24, &[OWNER]);
    new.file_ids = vec![id(34)];
    assert_eq!(send(&app, REPORTER, new).await, Ok(StatusCode::CREATED));
    assert!(listed(&app, MODERATOR).await.unwrap().is_empty());
    // Even a key row for them (as an older app might have sealed) opens no file.
    query("INSERT INTO report_keys (report_id, device_id, wrapped) VALUES (?, ?, X'00')")
        .bind(id(1))
        .bind(device(MODERATOR))
        .execute(&app.db)
        .await?;
    let mut db = app.db.acquire().await?;
    assert!(!may_download(&mut db, &actor(MODERATOR), &id(34)).await?);
    let owner = may_download(&mut db, &actor(OWNER), &id(34)).await?;
    assert!(owner);
    drop(db);
    for action in [Resolution::Dismissed, Resolution::Deleted] {
        assert_eq!(
            settle(&app, MODERATOR, 1, action).await,
            Err("report_not_found")
        );
    }
    // The owner does.
    assert_eq!(listed(&app, OWNER).await.unwrap().len(), 1);
    assert_eq!(settle(&app, OWNER, 1, Resolution::Dismissed).await, Ok(()));
    Ok(())
}

/// More messages from the sender in the channel, IDs `from..to`.
async fn more_messages(app: &App, ids: std::ops::Range<u8>) -> Result<()> {
    for message in ids {
        query(
            "INSERT INTO messages (id, channel_id, epoch, sender_device, sender_person, at, size, envelope)
             VALUES (?, ?, 1, ?, ?, 0, 111, zeroblob(111))",
        )
        .bind(id(message))
        .bind(id(10))
        .bind(device(SENDER))
        .bind(person(SENDER))
        .execute(&app.db)
        .await?;
    }
    Ok(())
}

#[tokio::test]
async fn only_accepted_reports_use_the_rate_limit() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let mut app = seeded(dir.path()).await?;
    app.limits = Limits::for_tests(None, false).into();
    more_messages(&app, 60..72).await?;
    // Refused reports (a stale reviewer list, an unknown message) cost nothing (M1).
    for attempt in 0..15u8 {
        let stale = report(100 + attempt, 10, 60, &[OWNER]);
        assert_eq!(send(&app, REPORTER, stale).await, Err("reviewers_changed"));
        let missing = report(120 + attempt, 10, 99, &CHANNEL_REVIEWERS);
        assert_eq!(
            send(&app, REPORTER, missing).await,
            Err("message_not_found")
        );
    }
    // Ten accepted at once (the standard burst), then the limiter steps in.
    let mut codes = Vec::new();
    for (n, message) in (60..71u8).enumerate() {
        let new = report(140 + n as u8, 10, message, &CHANNEL_REVIEWERS);
        codes.push(send(&app, REPORTER, new).await);
    }
    assert!(
        codes[..10]
            .iter()
            .all(|code| *code == Ok(StatusCode::CREATED))
    );
    assert_eq!(codes[10], Err("rate_limited"));
    Ok(())
}

#[tokio::test]
async fn open_reports_are_capped_apart_for_dms_and_channels() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let app = seeded(dir.path()).await?;
    let open = |who: u8, n: u32, dm: bool| {
        query(
            "INSERT INTO reports (id, reporter_id, channel_id, message_id, message_device,
               message_epoch, message_hash, sender_id, dm, body_ct, created_at)
             VALUES (?, ?, 'c', ?, X'00', 1, X'00', X'00', ?, X'00', 0)",
        )
        .bind(format!("r{who}-{n}-{dm}"))
        .bind(person(who))
        .bind(format!("m{n}"))
        .bind(dm)
    };
    // Twenty open channel reports per member...
    for n in 0..20 {
        open(REPORTER, n, false).execute(&app.db).await?;
    }
    assert_eq!(
        send(&app, REPORTER, report(1, 10, 20, &CHANNEL_REVIEWERS)).await,
        Err("too_many_reports")
    );
    // ...don't block a DM report (M5).
    assert_eq!(
        send(&app, REPORTER, report(2, 11, 23, &[OWNER])).await,
        Ok(StatusCode::CREATED)
    );
    // A thousand open channel reports on the server fill the channel queue only.
    for n in 0..980 {
        open(OUTSIDER, n, false).execute(&app.db).await?;
    }
    assert_eq!(
        send(&app, OWNER, report(3, 10, 20, &[MODERATOR, OWNER])).await,
        Err("report_queue_full")
    );
    // Reports by a banned member stay listed until handled (round 3 H1) and still
    // count, so the queue never outgrows what the app lists (round 4 H1)...
    query("INSERT INTO bans (person_id) VALUES (?)")
        .bind(person(OUTSIDER))
        .execute(&app.db)
        .await?;
    assert_eq!(
        send(&app, OWNER, report(3, 10, 20, &[MODERATOR, OWNER])).await,
        Err("report_queue_full")
    );
    let shown = listed(&app, MODERATOR).await.unwrap();
    assert!(
        shown
            .iter()
            .any(|report| report.reporter == encode(&person(OUTSIDER)))
    );
    // ...until a reviewer dismisses them in one go.
    let closed = dismiss_from(
        State(app.clone()),
        as_person(MODERATOR),
        Path(encode(&person(OUTSIDER))),
        Ok(Query(DismissKind { dm: false })),
    )
    .await
    .unwrap();
    assert_eq!(closed.0.reports, 980);
    assert_eq!(
        send(&app, OWNER, report(3, 10, 20, &[MODERATOR, OWNER])).await,
        Ok(StatusCode::CREATED)
    );
    // DM reports are listed on their own, so channel reports can't push them out of
    // reach (M1).
    let dms = page(&app, OWNER, None, Some(true)).await.unwrap();
    assert_eq!(dms.len(), 1);
    assert!(dms[0].dm && dms[0].id == id(2));
    let channels = page(&app, OWNER, None, Some(false)).await.unwrap();
    assert!(channels.len() == 21 && channels.iter().all(|report| !report.dm));
    Ok(())
}

#[tokio::test]
async fn the_owner_dismisses_a_members_dm_reports_at_once() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let app = seeded(dir.path()).await?;
    send(&app, REPORTER, report(1, 11, 23, &[OWNER]))
        .await
        .unwrap();
    send(&app, REPORTER, report(2, 10, 20, &CHANNEL_REVIEWERS))
        .await
        .unwrap();
    let dismiss = |who: u8, dm: bool| {
        dismiss_from(
            State(app.clone()),
            as_person(who),
            Path(encode(&person(REPORTER))),
            Ok(Query(DismissKind { dm })),
        )
    };
    // DM reports are the owner's (H1): a moderator can't, and only those go.
    assert_eq!(
        dismiss(MODERATOR, true)
            .await
            .map_err(|error| error.code)
            .err(),
        Some("permission_denied")
    );
    let closed = dismiss(OWNER, true).await.unwrap();
    assert_eq!(closed.0.reports, 1);
    let left = listed(&app, OWNER).await.unwrap();
    assert_eq!(left.len(), 1);
    assert!(!left[0].dm);
    // Audited for the owner's eyes only, under the Reports filter.
    query("INSERT INTO member_roles (person_id, role_id) VALUES (?, 'admin')")
        .bind(person(MODERATOR))
        .execute(&app.db)
        .await?;
    let actions = async |who: u8| -> Vec<String> {
        let query = Query(serde_json::from_value(json!({ "action": "report_resolved" })).unwrap());
        let read = crate::members::audit_log(State(app.clone()), as_person(who), Ok(query));
        let entries = serde_json::to_value(read.await.unwrap().0).unwrap();
        (entries.as_array().unwrap().iter())
            .map(|entry| entry["action"].as_str().unwrap().to_owned())
            .collect()
    };
    assert_eq!(actions(OWNER).await, ["dm_reports_dismissed"]);
    assert!(actions(MODERATOR).await.is_empty());
    Ok(())
}

#[tokio::test]
async fn dismissing_all_skips_reports_about_yourself_unless_owner() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let app = seeded(dir.path()).await?;
    for (n, reporter, sender) in [
        (1, OUTSIDER, OWNER),
        (2, OUTSIDER, MODERATOR),
        (3, OUTSIDER, SENDER),
        (4, REPORTER, OWNER),
        (5, REPORTER, OWNER),
    ] {
        query(
            "INSERT INTO reports (id, reporter_id, channel_id, message_id, message_device,
               message_epoch, message_hash, sender_id, dm, body_ct, created_at)
             VALUES (?, ?, ?, ?, X'00', 1, X'00', ?, FALSE, X'00', ?)",
        )
        .bind(id(n))
        .bind(person(reporter))
        .bind(id(10))
        .bind(format!("m{n}"))
        .bind(person(sender))
        .bind(i64::from(n))
        .execute(&app.db)
        .await?;
    }
    let dismiss = async |who: u8, reporter: u8| {
        dismiss_from(
            State(app.clone()),
            as_person(who),
            Path(encode(&person(reporter))),
            Ok(Query(DismissKind { dm: false })),
        )
        .await
        .unwrap()
        .0
        .reports
    };
    // A moderator can't bury a report about themselves, and is told what closed.
    assert_eq!(dismiss(MODERATOR, OUTSIDER).await, 2);
    // The owner, who handles reports about themselves anyway, clears a raid on their
    // own messages in one go (round 5 L1).
    assert_eq!(dismiss(OWNER, REPORTER).await, 2);
    assert_eq!(dismiss(OWNER, REPORTER).await, 0);
    let open: Vec<String> =
        query_scalar("SELECT id FROM reports WHERE resolved_at IS NULL ORDER BY id")
            .fetch_all(&app.db)
            .await?;
    assert_eq!(open, [id(2)]);
    Ok(())
}

#[tokio::test]
async fn closing_a_report_with_an_action_needs_that_permission() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let app = seeded(dir.path()).await?;
    // A reviewer who may only review reports.
    let review_only = Permission::ReviewReports as i64;
    query("INSERT INTO roles (id, kind, rank, permissions) VALUES ('reviewer', 'custom', 20, ?)")
        .bind(review_only)
        .execute(&app.db)
        .await?;
    query("UPDATE member_roles SET role_id = 'reviewer' WHERE person_id = ?")
        .bind(person(MODERATOR))
        .execute(&app.db)
        .await?;
    send(&app, REPORTER, report(1, 10, 20, &CHANNEL_REVIEWERS))
        .await
        .unwrap();
    // Someone else banned the sender: still not theirs to claim, nor a kick or timeout
    // they may not give (L4).
    query("INSERT INTO bans (person_id) VALUES (?)")
        .bind(person(SENDER))
        .execute(&app.db)
        .await?;
    for action in [Resolution::Kicked, Resolution::Banned, Resolution::TimedOut] {
        assert_eq!(
            settle(&app, MODERATOR, 1, action).await,
            Err("permission_denied")
        );
    }
    assert_eq!(settle(&app, OWNER, 1, Resolution::Banned).await, Ok(()));
    Ok(())
}

#[tokio::test]
async fn banning_a_reporter_hides_none_of_their_reports() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let app = seeded(dir.path()).await?;
    // The reporter reports the moderator in the channel and the sender in their DM...
    send(&app, REPORTER, report(1, 10, 24, &[OWNER]))
        .await
        .unwrap();
    send(&app, REPORTER, report(2, 11, 23, &[OWNER]))
        .await
        .unwrap();
    // ...and the reported moderator bans the reporter (H1). The owner still sees both,
    // evidence and all, and so may still download their files.
    query("INSERT INTO bans (person_id, banned_by) VALUES (?, ?)")
        .bind(person(REPORTER))
        .bind(person(MODERATOR))
        .execute(&app.db)
        .await?;
    let shown = listed(&app, OWNER).await.unwrap();
    let ids: Vec<&str> = shown.iter().map(|report| report.id.as_str()).collect();
    assert_eq!(ids, [id(1), id(2)]);
    assert!(shown.iter().all(|report| report.body == encode(&[0; 200])));
    Ok(())
}

#[tokio::test]
async fn dismissing_one_report_keeps_the_others_of_that_message() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let app = seeded(dir.path()).await?;
    // A bogus report first, then a real one of the same message.
    send(&app, REPORTER, report(1, 10, 20, &CHANNEL_REVIEWERS))
        .await
        .unwrap();
    send(&app, OWNER, report(2, 10, 20, &[MODERATOR, OWNER]))
        .await
        .unwrap();
    // Dismissing the bogus one leaves the other and its evidence (H2).
    settle(&app, MODERATOR, 1, Resolution::Dismissed)
        .await
        .unwrap();
    let left = listed(&app, MODERATOR).await.unwrap();
    assert_eq!(left.len(), 1);
    assert_eq!(left[0].id, id(2));
    assert_eq!(left[0].body, encode(&[0; 200]));
    assert_eq!(left[0].key, Some(encode(&[MODERATOR; WRAP_BYTES])));
    Ok(())
}

#[tokio::test]
async fn the_queue_pages_and_a_spammers_reports_go_at_once() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let app = seeded(dir.path()).await?;
    for n in 0..150u32 {
        query(
            "INSERT INTO reports (id, reporter_id, channel_id, message_id, message_device,
               message_epoch, message_hash, sender_id, dm, body_ct, created_at)
             VALUES (?, ?, ?, ?, X'00', 1, X'00', ?, FALSE, X'00', ?)",
        )
        .bind(format!("{n:04}"))
        .bind(person(OUTSIDER))
        .bind(id(10))
        .bind(format!("m{n}"))
        .bind(person(SENDER))
        .bind(i64::from(n / 2))
        .execute(&app.db)
        .await?;
    }
    // Oldest first, a hundred at a time, continuing after the last one listed.
    let first = page(&app, MODERATOR, None, None).await.unwrap();
    assert_eq!(first.len(), 100);
    let last = first.last().unwrap();
    let rest = page(&app, MODERATOR, Some((last.at, last.id.clone())), None)
        .await
        .unwrap();
    assert_eq!(rest.len(), 50);
    let ids: HashSet<String> = first.iter().chain(&rest).map(|r| r.id.clone()).collect();
    assert_eq!(ids.len(), 150);

    // One click dismisses everything a spammer reported in channels, and is audited.
    let dismiss = |who: u8| {
        dismiss_from(
            State(app.clone()),
            as_person(who),
            Path(encode(&person(OUTSIDER))),
            Ok(Query(DismissKind { dm: false })),
        )
    };
    assert_eq!(
        dismiss(REPORTER).await.map_err(|error| error.code).err(),
        Some("permission_denied")
    );
    let closed = dismiss(MODERATOR).await.unwrap();
    assert_eq!(closed.0.reports, 150);
    assert!(listed(&app, MODERATOR).await.unwrap().is_empty());
    let audited: String =
        query_scalar("SELECT details FROM audit_log WHERE action = 'reports_dismissed'")
            .fetch_one(&app.db)
            .await?;
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&audited)?,
        json!({"reports": 150})
    );
    // The audit log's Reports filter lists it.
    let query = Query(serde_json::from_value(
        json!({ "action": "report_resolved" }),
    )?);
    let filtered = crate::members::audit_log(State(app.clone()), as_person(OWNER), Ok(query))
        .await
        .unwrap();
    let filtered = serde_json::to_value(filtered.0)?;
    assert_eq!(filtered[0]["action"], "reports_dismissed");
    Ok(())
}

#[tokio::test]
async fn removed_reviewer_devices_are_left_out_only_with_their_signed_removal() -> Result<()> {
    use rusttalk_core::identity::{DeviceIdentity, revoke_device};
    let dir = tempfile::tempdir()?;
    let app = seeded(dir.path()).await?;
    // A second moderator with a real root, and two devices.
    let (words, lead) = DeviceIdentity::create("lead").unwrap();
    let lead_person = *lead.certificate.person_id();
    let (_, spare) = DeviceIdentity::create("spare").unwrap();
    let spare_device = spare.certificate.signing_key().to_bytes();
    query("INSERT INTO members (person_id, joined_at) VALUES (?, 0)")
        .bind(lead_person.as_slice())
        .execute(&app.db)
        .await?;
    query("INSERT INTO member_roles (person_id, role_id) VALUES (?, 'moderator')")
        .bind(lead_person.as_slice())
        .execute(&app.db)
        .await?;
    for device in [lead.certificate.signing_key().to_bytes(), spare_device] {
        query(
            "INSERT INTO member_devices (device_id, person_id, certificate) VALUES (?, ?, X'00')",
        )
        .bind(device.as_slice())
        .bind(lead_person.as_slice())
        .execute(&app.db)
        .await?;
    }
    let with_keys = |devices: Vec<Vec<u8>>| {
        let mut new = report(1, 10, 20, &CHANNEL_REVIEWERS);
        new.keys.extend(devices.into_iter().map(|device| ReportKey {
            device: encode(&device),
            key: encode(&[9; WRAP_BYTES]),
        }));
        new
    };
    let lead_device = lead.certificate.signing_key().to_bytes().to_vec();
    // Leaving the spare device out needs its removal, signed by its owner's root.
    let mut silent = with_keys(vec![lead_device.clone()]);
    assert_eq!(send(&app, REPORTER, silent).await, Err("reviewers_changed"));
    let removal = revoke_device(&words, &lead_person, spare_device).unwrap();
    let signed = |person: [u8; 32], record: Vec<u8>| SignedRevocation {
        person: encode(&person),
        record: encode(&record),
    };
    // Someone else's signature over it proves nothing.
    silent = with_keys(vec![lead_device.clone()]);
    silent.omitted = vec![signed([3; 32], removal.to_bytes())];
    assert_eq!(
        send(&app, REPORTER, silent).await,
        Err("invalid_revocation")
    );
    let mut proven = with_keys(vec![lead_device.clone()]);
    proven.omitted = vec![signed(lead_person, removal.to_bytes())];
    assert_eq!(send(&app, REPORTER, proven).await, Ok(StatusCode::CREATED));
    // The removal reaching the server first doesn't fail a report proving it (M3).
    query("DELETE FROM member_devices WHERE device_id = ?")
        .bind(spare_device.as_slice())
        .execute(&app.db)
        .await?;
    let mut raced = with_keys(vec![lead_device]);
    (raced.id, raced.message_id) = (id(2), id(25));
    raced.omitted = vec![signed(lead_person, removal.to_bytes())];
    assert_eq!(send(&app, REPORTER, raced).await, Ok(StatusCode::CREATED));
    Ok(())
}

#[tokio::test]
async fn signed_removals_are_checked_only_for_a_session() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let app = seeded(dir.path()).await?;
    // Without a session, nothing is verified (M2): the caller is refused first.
    let flood = || {
        let mut new = report(1, 10, 20, &CHANNEL_REVIEWERS);
        new.omitted = vec![
            SignedRevocation {
                person: id(7),
                record: encode(&[0; 100]),
            };
            200
        ];
        new
    };
    let refused = create(State(app.clone()), HeaderMap::new(), Ok(Json(flood()))).await;
    assert_eq!(
        refused.err().map(|error| error.code),
        Some("invalid_session")
    );
    assert_eq!(
        send(&app, REPORTER, flood()).await,
        Err("invalid_revocation")
    );
    Ok(())
}

#[tokio::test]
async fn reviewers_get_the_device_removals_of_sender_and_reporter() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let app = seeded(dir.path()).await?;
    send(&app, REPORTER, report(1, 10, 20, &CHANNEL_REVIEWERS))
        .await
        .unwrap();
    send(&app, REPORTER, report(2, 11, 23, &[OWNER]))
        .await
        .unwrap();
    // The sender's and reporter's removals are served with each report, from the
    // server's own list rather than only members' (M4); unrelated people's are not.
    for (who, device) in [(SENDER, 1u8), (OUTSIDER, 2), (REPORTER, 3)] {
        query("INSERT INTO revocations (person_id, device_id, record) VALUES (?, ?, ?)")
            .bind(person(who))
            .bind(vec![device; 32])
            .bind(vec![who; 4])
            .execute(&app.db)
            .await?;
    }
    let theirs = |who: u8| SignedRevocation {
        person: encode(&person(who)),
        record: encode(&[who; 4]),
    };
    // Looked up once for the whole page (L5), each report getting its own people's.
    let shown = listed(&app, OWNER).await.unwrap();
    assert_eq!(shown.len(), 2);
    for report in shown {
        assert_eq!(report.revocations, [theirs(SENDER), theirs(REPORTER)]);
    }
    Ok(())
}

#[tokio::test]
async fn a_message_with_a_shared_nonce_prefix_cant_be_reported() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let app = seeded(dir.path()).await?;
    // As the upgrade marks prefixes two stored records already shared (L1).
    query("INSERT INTO nonce_prefixes (channel_id, epoch, prefix, reused) VALUES (?, 1, ?, TRUE)")
        .bind(id(10))
        .bind(vec![0u8; 16])
        .execute(&app.db)
        .await?;
    assert_eq!(
        send(&app, REPORTER, report(1, 10, 20, &CHANNEL_REVIEWERS)).await,
        Err("nonce_reused")
    );
    Ok(())
}

#[tokio::test]
async fn a_message_can_be_reported_again_after_its_report_closed() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let app = seeded(dir.path()).await?;
    send(&app, REPORTER, report(1, 10, 20, &CHANNEL_REVIEWERS))
        .await
        .unwrap();
    assert_eq!(
        send(&app, REPORTER, report(2, 10, 20, &CHANNEL_REVIEWERS)).await,
        Err("already_reported")
    );
    settle(&app, MODERATOR, 1, Resolution::Dismissed)
        .await
        .unwrap();
    // Say after a worse edit (L2).
    let mut again = report(2, 10, 20, &CHANNEL_REVIEWERS);
    again.edit_id = Some(id(21));
    assert_eq!(send(&app, REPORTER, again).await, Ok(StatusCode::CREATED));
    Ok(())
}

#[tokio::test]
async fn a_reused_nonce_prefix_is_refused_within_a_channel_epoch() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let app = seeded(dir.path()).await?;
    let mut db = app.db.acquire().await?;
    let envelope = |byte: u8| [vec![3], vec![byte; 16], vec![0; 100]].concat();
    let reserve = async |db: &mut SqliteConnection, channel: u8, envelope: Vec<u8>| {
        messages::reserve_nonce(db, &id(channel), 1, &envelope)
            .await
            .map_err(|error| error.code)
    };
    assert_eq!(reserve(&mut db, 10, envelope(1)).await, Ok(()));
    assert_eq!(reserve(&mut db, 10, envelope(1)).await, Err("nonce_reused"));
    // Another prefix, or the same in another channel, is fine.
    assert_eq!(reserve(&mut db, 10, envelope(2)).await, Ok(()));
    assert_eq!(reserve(&mut db, 12, envelope(1)).await, Ok(()));
    Ok(())
}

#[tokio::test]
async fn handling_a_report_is_checked_and_audited() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let app = seeded(dir.path()).await?;
    let mut first = report(1, 10, 20, &CHANNEL_REVIEWERS);
    first.edit_id = Some(id(21));
    first.file_ids = vec![id(30)];
    send(&app, REPORTER, first).await.unwrap();
    // Another report of the same message, as the owner saw it with a later edit.
    let mut second = report(2, 10, 20, &[MODERATOR, OWNER]);
    second.edit_id = Some(id(25));
    send(&app, OWNER, second).await.unwrap();
    send(&app, REPORTER, report(3, 11, 23, &[OWNER]))
        .await
        .unwrap();

    // A kick or ban must have happened before it can close a report.
    for action in [Resolution::Kicked, Resolution::Banned, Resolution::TimedOut] {
        assert_eq!(
            settle(&app, MODERATOR, 1, action).await,
            Err("action_not_taken")
        );
    }
    assert_eq!(
        settle(&app, REPORTER, 1, Resolution::Dismissed).await,
        Err("report_not_found")
    );
    // Deleting removes the message and the edit this report showed, which the reviewer
    // verified, and its files that both the reviewer and the report name (L2). Another
    // report naming a different message of the sender as its edit can't get that one
    // deleted too, and stays open while that edit is there.
    assert_eq!(
        settle_with(
            &app,
            MODERATOR,
            1,
            Resolution::Deleted,
            [30, 34].map(id).to_vec()
        )
        .await,
        Ok(())
    );
    let kept: Vec<String> = query_scalar("SELECT id FROM files WHERE id IN (?, ?) ORDER BY id")
        .bind(id(30))
        .bind(id(34))
        .fetch_all(&app.db)
        .await?;
    assert_eq!(kept, [id(34)], "only the reported message's file goes");
    let left = async |message: u8| -> Result<bool> {
        Ok(
            query_scalar("SELECT EXISTS (SELECT 1 FROM messages WHERE id = ?)")
                .bind(id(message))
                .fetch_one(&app.db)
                .await?,
        )
    };
    assert!(!left(20).await? && !left(21).await?);
    assert!(
        left(25).await?,
        "an edit named only by another report stays"
    );
    let open = listed(&app, MODERATOR).await.unwrap();
    assert_eq!(
        open.iter().map(|r| r.id.clone()).collect::<Vec<_>>(),
        [id(2)]
    );
    assert_eq!(
        settle(&app, MODERATOR, 1, Resolution::Dismissed).await,
        Err("report_resolved")
    );
    // Deleting that one too removes its edit; the original is already gone.
    assert_eq!(
        settle(&app, MODERATOR, 2, Resolution::Deleted).await,
        Ok(())
    );
    assert!(!left(25).await?);
    // The evidence and its keys go; the row stays for duplicates and the log.
    let (body, keys): (Vec<u8>, i64) =
        query_as("SELECT body_ct, (SELECT COUNT(*) FROM report_keys) FROM reports WHERE id = ?")
            .bind(id(1))
            .fetch_one(&app.db)
            .await?;
    assert!(
        body.is_empty() && keys == 1,
        "only the open DM report keeps its key"
    );

    // DM reports are the owner's; the owner can't delete a DM message.
    assert_eq!(
        settle(&app, MODERATOR, 3, Resolution::Dismissed).await,
        Err("report_not_found")
    );
    assert_eq!(
        settle(&app, OWNER, 3, Resolution::Deleted).await,
        Err("permission_denied")
    );
    query("INSERT INTO bans (person_id) VALUES (?)")
        .bind(person(SENDER))
        .execute(&app.db)
        .await?;
    assert_eq!(settle(&app, OWNER, 3, Resolution::Banned).await, Ok(()));

    type Entry = (Vec<u8>, String, Option<String>, Option<String>);
    let audit: Vec<Entry> =
        query_as("SELECT actor_person_id, action, target_id, details FROM audit_log ORDER BY id")
            .fetch_all(&app.db)
            .await?;
    let actions: Vec<&str> = audit
        .iter()
        .map(|(_, action, ..)| action.as_str())
        .collect();
    assert_eq!(
        actions,
        [
            "message_deleted",
            "message_deleted",
            "report_resolved",
            "message_deleted",
            "report_resolved",
            "dm_report_resolved"
        ]
    );
    assert_eq!(audit[2].0, person(MODERATOR));
    assert_eq!(audit[2].2, Some(id(1)));
    let details: serde_json::Value = serde_json::from_str(audit[2].3.as_deref().unwrap())?;
    assert_eq!(
        details,
        json!({ "resolution": "deleted", "reports": 1, "files": 1 })
    );

    // Only the owner reads how a DM report was handled (L2).
    query("INSERT INTO member_roles (person_id, role_id) VALUES (?, 'admin')")
        .bind(person(MODERATOR))
        .execute(&app.db)
        .await?;
    let read = |who: u8| {
        let query = Query(serde_json::from_value(json!({})).unwrap());
        crate::members::audit_log(State(app.clone()), as_person(who), Ok(query))
    };
    let actions = |entries: Vec<serde_json::Value>| -> Vec<String> {
        (entries.iter())
            .map(|entry| entry["action"].as_str().unwrap().to_owned())
            .collect()
    };
    let admin = serde_json::to_value(read(MODERATOR).await.unwrap().0)?;
    let owner = serde_json::to_value(read(OWNER).await.unwrap().0)?;
    let admin = actions(serde_json::from_value(admin)?);
    let owner = actions(serde_json::from_value(owner)?);
    assert!(admin.contains(&"report_resolved".to_owned()));
    assert!(!admin.contains(&"dm_report_resolved".to_owned()));
    assert!(owner.contains(&"dm_report_resolved".to_owned()));
    // The Reports filter covers DM reports, for the owner only.
    let filtered = |who: u8| {
        let query = json!({ "action": "report_resolved" });
        let query = Query(serde_json::from_value(query).unwrap());
        crate::members::audit_log(State(app.clone()), as_person(who), Ok(query))
    };
    let owner = serde_json::to_value(filtered(OWNER).await.unwrap().0)?;
    let admin = serde_json::to_value(filtered(MODERATOR).await.unwrap().0)?;
    assert_eq!(
        actions(serde_json::from_value(owner)?),
        ["report_resolved", "report_resolved", "dm_report_resolved"]
    );
    assert_eq!(
        actions(serde_json::from_value(admin)?),
        ["report_resolved", "report_resolved"]
    );
    Ok(())
}
