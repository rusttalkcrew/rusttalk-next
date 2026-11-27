//! Reports (#678): reporting a message asks why and sends it; Server settings > Reports
//! shows each verified report with its sender, text and files, and the actions. Set
//! RUSTTALK_TEST_SCREENSHOTS to save how they look.

use super::*;
use crate::net::reports::Recipients;
use crate::net::{Admin, AdminUpdate, ReportAction};
use egui_kittest::{
    Harness,
    kittest::{NodeT, Queryable, by},
};
use rusttalk_core::{
    client::{Checked, MessageBody, ReceivedReport, Unreadable},
    identity::fingerprint,
    reports::Reason,
};

type Sent = tokio::sync::mpsc::UnboundedReceiver<Command>;

const SIZES: [Vec2; 2] = [Vec2::new(1100.0, 700.0), Vec2::new(390.0, 844.0)];
const SPAM: Id = [40; 32];

/// The whole screen with a message from Ann in #projects, as `change` leaves it.
fn screen(
    size: Vec2,
    dark: bool,
    change: impl FnOnce(&mut Home),
) -> (Harness<'static, Option<Screen>>, Sent) {
    let (mut files, sent) = files_harness(dark);
    let mut home = files.state_mut().take().unwrap();
    home.view = View::Server(0);
    home.servers[0].open = Some(PROJECTS);
    home.find.opened = false;
    home.layout.list = false;
    let chat = home.servers[0].chats.get_mut(&PROJECTS).unwrap();
    chat.messages.push(Msg {
        backfilled: false,
        id: SPAM,
        person: ANN,
        at: 1_700_000_000,
        reply_to: None,
        quoted_author: None,
        edit_of: None,
        body: Body::Text("Cheap watches, click here!".into()),
        files: Vec::new(),
        live: None,
        sticker: None,
        replicas: Vec::new(),
        copy_files: Vec::new(),
    });
    change(&mut home);
    let data = tempfile::tempdir().unwrap();
    let mut harness = Harness::builder()
        .with_size(size)
        .with_context_setup(ui::install)
        .with_theme(if dark {
            egui::Theme::Dark
        } else {
            egui::Theme::Light
        })
        .build_ui_state(
            |ui, screen: &mut Option<Screen>| {
                if let Some((home, updates, keystore)) = screen {
                    show(ui, home, updates, keystore);
                }
            },
            None,
        );
    *harness.state_mut() = Some((
        home,
        crate::update::Updater::new(data.path()),
        crate::keystore::without_keychain(data.path()),
    ));
    harness.run_steps(6);
    (harness, sent)
}

fn shot(h: &mut Harness<'_, Option<Screen>>, name: &str, size: Vec2, dark: bool) {
    if let Ok(dir) = std::env::var("RUSTTALK_TEST_SCREENSHOTS") {
        std::fs::create_dir_all(&dir).unwrap();
        let theme = if dark { "dark" } else { "light" };
        let path = format!("{dir}/{name}-{theme}-{}x{}.png", size.x, size.y);
        h.render().unwrap().save(path).unwrap();
    }
}

fn button(label: &str) -> egui_kittest::kittest::By<'_> {
    by().role(Role::Button).label(label)
}

#[test]
fn reporting_names_every_recipient_before_it_is_sent() {
    for size in SIZES {
        for dark in [true, false] {
            let (mut h, mut sent) = screen(size, dark, |home| {
                let chat = &home.servers[0].chats[&PROJECTS];
                let draft = report::in_channel(0, PROJECTS, chat, (SPAM, ANN), false).unwrap();
                home.dialog = Some(Dialog::Report(report::open(&home.servers, draft)));
            });
            assert!(
                h.query_by_label(t!("reports.title")).is_some(),
                "at {size:?}"
            );
            // Opening asks who it goes to, and nothing can be sent until it's known.
            let asked: Vec<_> = std::iter::from_fn(|| sent.try_recv().ok())
                .filter_map(|command| match command {
                    Command::ReportRecipients {
                        channel,
                        message,
                        dm,
                        sender,
                    } => Some((channel, message, dm, sender)),
                    _ => None,
                })
                .collect();
            assert_eq!(asked, [(PROJECTS, SPAM, false, ANN)]);
            h.get_by_label(t!("reports.reason_spam")).click();
            h.run_steps(3);
            let send = button(t!("reports.send"));
            assert!(h.get(send).accesskit_node().is_disabled());
            // The names arrive, with a warning that the owner changed.
            let ctx = h.ctx.clone();
            let (home, ..) = h.state_mut().as_mut().unwrap();
            let address = home.servers[0].address.clone();
            let found = Recipients {
                people: vec![ME, ANN],
                previous_owner: Some(ANN),
                owner: Some(ME),
            };
            home.apply(
                &ctx,
                Update::ReportRecipients(address, SPAM, Ok(found)),
                0.0,
            );
            h.run_steps(3);
            // Each with their whole fingerprint, as names needn't be unique (round 3 H3)
            // and a short one could be ground out (round 4 L1).
            let ann = format!("Ann ({})", fingerprint(&ANN));
            let names = format!("Me ({}), {ann}", fingerprint(&ME));
            let names = t!("reports.recipients", names = names);
            assert!(h.query_by_label(&names).is_some(), "recipients at {size:?}");
            let changed = t!("reports.owner_changed", before = ann);
            assert!(h.query_by_label(&changed).is_some());
            shot(&mut h, "report-dialog", size, dark);
            h.get(button(t!("reports.send"))).click();
            h.run_steps(3);
            let reports: Vec<_> = std::iter::from_fn(|| sent.try_recv().ok())
                .filter_map(|command| match command {
                    Command::Report(report) => Some((
                        report.channel,
                        report.message,
                        report.dm,
                        report.reason,
                        report.recipients,
                        report.owner,
                    )),
                    _ => None,
                })
                .collect();
            // With the owner shown, so nothing goes out if another is the owner by then.
            assert_eq!(
                reports,
                [(PROJECTS, SPAM, false, Reason::Spam, vec![ME, ANN], Some(ME))]
            );
            assert!(h.query_by_label(t!("reports.title")).is_none());
        }
    }
}

#[test]
fn a_report_is_not_sent_when_the_recipients_cant_be_checked() {
    let (h, _) = screen(SIZES[0], true, |home| {
        let chat = &home.servers[0].chats[&PROJECTS];
        let mut draft = report::in_channel(0, PROJECTS, chat, (SPAM, ANN), false).unwrap();
        draft.reason = Some(Reason::Spam);
        draft.recipients = Some(Err(t!("reports.recipients_unchecked").into()));
        home.dialog = Some(Dialog::Report(draft));
    });
    assert!(
        h.query_by_label(t!("reports.recipients_unchecked"))
            .is_some()
    );
    assert!(
        h.get(button(t!("reports.send")))
            .accesskit_node()
            .is_disabled()
    );
}

fn received(dark: bool, content: Result<Checked, Unreadable>) -> ReceivedReport {
    ReceivedReport {
        id: [if dark { 50 } else { 51 }; 32],
        channel: PROJECTS,
        message: SPAM,
        reporter: ME,
        sender: ANN,
        dm: false,
        at: 1_700_000_000,
        content,
    }
}

fn verified() -> Checked {
    let file = rusttalk_core::client::prepare_file("watch.png", "image/png", &[0; 2048], &[])
        .unwrap()
        .0;
    Checked {
        reason: Reason::Spam,
        note: "Posted in every channel".into(),
        body: MessageBody {
            text: "Cheap watches, 90% off today!".into(),
            attachments: vec![file],
            ..Default::default()
        },
        sender_removed: false,
        confirmation: rusttalk_core::reports::Confirmation::Matched,
    }
}

fn reviewer(home: &mut Home) {
    let perms = &mut home.servers[0].dir.perms;
    perms.review_reports = true;
    perms.delete_messages = true;
    perms.timeout_members = true;
    perms.kick = true;
    perms.ban = true;
    perms.rank = 50;
    crate::admin::open(home, 0, crate::admin::Tab::Reports);
}

fn handled(sent: &mut Sent) -> Vec<(Id, ReportAction, Vec<Id>)> {
    std::iter::from_fn(|| sent.try_recv().ok())
        .filter_map(|command| match command {
            Command::Admin(Admin::HandleReport {
                sender,
                action,
                files,
                ..
            }) => Some((sender, action, files)),
            _ => None,
        })
        .collect()
}

#[test]
fn the_reports_page_shows_verified_reports_and_actions() {
    for size in SIZES {
        for dark in [true, false] {
            let (mut h, mut sent) = screen(size, dark, |home| {
                reviewer(home);
                let list = vec![
                    received(dark, Ok(verified())),
                    received(!dark, Err(Unreadable::Invalid("bad signature".into()))),
                ];
                let settings = home.server_settings.as_mut().unwrap();
                settings.apply(AdminUpdate::Reports(list));
            });
            if h.query_by_label(t!("reports.page_help")).is_none() {
                // Phones may start on the page list.
                h.get_by_label(t!("reports.page")).click();
                h.run_steps(4);
            }
            let verified_label = t!("reports.verified", sender = "Ann");
            assert!(h.query_by_label(&verified_label).is_some(), "at {size:?}");
            assert!(h.query_by_label(t!("reports.unverified")).is_some());
            assert!(h.query_by_label("Cheap watches, 90% off today!").is_some());
            assert!(h.query_by_label_contains("watch.png").is_some());
            // Only the verified report can be acted on; both can be dismissed.
            assert_eq!(h.query_all(button(t!("reports.delete"))).count(), 1);
            assert_eq!(h.query_all(button(t!("reports.dismiss"))).count(), 2);
            shot(&mut h, "reports-page", size, dark);

            // Deleting asks first.
            h.get(button(t!("reports.delete"))).click();
            h.run_steps(4);
            assert!(h.query_by_label(t!("reports.delete_title")).is_some());
            assert!(handled(&mut sent).is_empty());
            let confirm = h.query_all(button(t!("reports.delete"))).last().unwrap();
            confirm.click();
            h.run_steps(4);
            // With the verified message's file, deleted along with it (L2).
            let asked = handled(&mut sent);
            assert!(
                matches!(asked.as_slice(), [(ANN, ReportAction::Delete, files)] if files.len() == 1)
            );
        }
    }
}

#[test]
fn only_reviewers_see_the_reports_page() {
    let (h, _) = screen(SIZES[0], true, |home| {
        crate::admin::open(home, 0, crate::admin::Tab::Files);
    });
    assert!(h.query_by_label(t!("reports.page")).is_none());
    let (h, _) = screen(SIZES[0], true, |home| {
        home.servers[0].dir.perms.review_reports = true;
        crate::admin::open(home, 0, crate::admin::Tab::Files);
    });
    assert!(h.query_by_label(t!("reports.page")).is_some());
    // An owner on a server from before reports has no such page (L3).
    let (h, _) = screen(SIZES[0], true, |home| {
        home.servers[0].dir.perms.owner = true;
        home.servers[0].dir.perms.review_reports = false;
        crate::admin::open(home, 0, crate::admin::Tab::Files);
    });
    assert!(h.query_by_label(t!("reports.page")).is_none());
}

#[test]
fn a_failed_load_of_reports_stops_the_spinner() {
    let (h, _) = screen(SIZES[0], true, |home| {
        reviewer(home);
        let settings = home.server_settings.as_mut().unwrap();
        settings.apply(AdminUpdate::Failed("That didn't work.".into()));
    });
    // The error shows instead of loading forever (L3).
    assert!(h.query_by_label("That didn't work.").is_some());
}

#[test]
fn a_dm_report_goes_through_a_server_that_holds_the_dm_and_is_online() {
    let (mut fixture, _) = files_harness(true);
    let mut home = fixture.state_mut().take().unwrap();
    const ANN_DM: Id = [9; 32];
    home.servers[0].dir.dms.push(crate::server::Dm {
        channel: ANN_DM,
        peer: ANN,
    });
    let known = KnownServer {
        address: "https://camp.example.org/".parse().unwrap(),
        server_id: [4; 32],
        tls_fingerprint: None,
    };
    let mut camp = new_server(known, "Camp", home.servers[0].commands.clone());
    camp.status = Status::Online;
    camp.dir = home.servers[0].dir.clone();
    camp.dir.name = Some("Camp".into());
    home.servers.push(camp);
    home.servers[0].status = Status::Online;
    let mut chat = Chat::default();
    // An older message of mine under the same ID, from the first server (M1).
    chat.messages.push(Msg {
        backfilled: false,
        id: SPAM,
        person: ME,
        at: 1_600_000_000,
        reply_to: None,
        quoted_author: None,
        edit_of: None,
        body: Body::Text("Hi Ann".into()),
        files: Vec::new(),
        live: None,
        sticker: None,
        replicas: vec![home.servers[0].address.clone()],
        copy_files: Vec::new(),
    });
    chat.messages.push(Msg {
        backfilled: false,
        id: SPAM,
        person: ANN,
        at: 1_700_000_000,
        reply_to: None,
        quoted_author: None,
        edit_of: None,
        body: Body::Text("Cheap watches, click here!".into()),
        files: Vec::new(),
        live: None,
        sticker: None,
        replicas: vec![
            home.servers[0].address.clone(),
            home.servers[1].address.clone(),
        ],
        copy_files: Vec::new(),
    });
    let draft = |home: &Home| report::draft(&home.servers, At::Dm(ANN), &chat, (SPAM, ANN));
    let server = |home: &Home| draft(home).map(|d| d.server);
    // Ann's message, not mine with the same ID.
    assert_eq!(draft(&home).map(|d| d.sender), Some(ANN));
    assert_eq!(server(&home), Some(0));
    assert_eq!(draft(&home).unwrap().choices.len(), 2);
    // A server that knows reports comes first, then one Ann doesn't own (M2).
    let reviews = |home: &mut Home, at: usize| {
        home.servers[at].dir.roles.push(crate::server::Role {
            id: "owner".into(),
            name: "Owner".into(),
            name_known: true,
            rank: 1000,
            permissions: vec!["review_reports".into()],
            custom: false,
            color: None,
            separate: false,
        });
    };
    reviews(&mut home, 1);
    assert_eq!(server(&home), Some(1));
    reviews(&mut home, 0);
    assert_eq!(server(&home), Some(0));
    let mut members = home.servers[0].dir.members.edit();
    let ann = members.iter_mut().find(|m| m.person == ANN).unwrap();
    ann.roles = vec!["owner".into()];
    drop(members);
    assert_eq!(server(&home), Some(1));
    // The shown copy's server offline, or left, or without the DM: the other one (M4).
    home.servers[0].status = Status::Offline;
    assert_eq!(server(&home), Some(1));
    home.servers[0].status = Status::Online;
    home.servers[0].dir.dms.clear();
    assert_eq!(server(&home), Some(1));
    home.servers[1].status = Status::Offline;
    assert_eq!(server(&home), None);
}

#[test]
fn the_reporter_picks_which_servers_owner_gets_a_dm_report() {
    const ANN_DM: Id = [9; 32];
    let (mut h, mut sent) = screen(SIZES[0], true, |home| {
        home.servers[0].dir.dms.push(crate::server::Dm {
            channel: ANN_DM,
            peer: ANN,
        });
        let known = KnownServer {
            address: "https://camp.example.org/".parse().unwrap(),
            server_id: [4; 32],
            tls_fingerprint: None,
        };
        let mut camp = new_server(known, "Camp", home.servers[0].commands.clone());
        camp.status = Status::Online;
        camp.dir = home.servers[0].dir.clone();
        camp.dir.name = Some("Camp".into());
        home.servers.push(camp);
        let chat = &home.servers[0].chats[&PROJECTS];
        let mut draft = report::in_channel(0, ANN_DM, chat, (SPAM, ANN), true).unwrap();
        draft.choices = vec![(0, ANN_DM), (1, ANN_DM)];
        home.dialog = Some(Dialog::Report(draft));
    });
    // Picking the other server asks its reviewers, and the dialog stays (M2).
    h.get(by().role(Role::RadioButton).label("Camp")).click();
    h.run_steps(3);
    let asked: Vec<_> = std::iter::from_fn(|| sent.try_recv().ok())
        .filter_map(|command| match command {
            Command::ReportRecipients { channel, dm, .. } => Some((channel, dm)),
            _ => None,
        })
        .collect();
    assert_eq!(asked, [(ANN_DM, true)]);
    assert!(h.query_by_label(t!("reports.title")).is_some());
    let (home, ..) = h.state_mut().as_ref().unwrap();
    let Some(Dialog::Report(draft)) = &home.dialog else {
        panic!("the report dialog closed");
    };
    assert_eq!(draft.server, 1);
}

#[test]
fn a_removed_sender_device_shows_a_warning_not_a_tick() {
    for size in SIZES {
        for dark in [true, false] {
            let (mut h, _) = screen(size, dark, |home| {
                reviewer(home);
                let removed = Checked {
                    sender_removed: true,
                    ..verified()
                };
                let settings = home.server_settings.as_mut().unwrap();
                settings.apply(AdminUpdate::Reports(vec![received(dark, Ok(removed))]));
            });
            if h.query_by_label(t!("reports.page_help")).is_none() {
                h.get_by_label(t!("reports.page")).click();
                h.run_steps(4);
            }
            let warning = t!("reports.device_removed", sender = "Ann");
            assert!(h.query_by_label(&warning).is_some(), "at {size:?}");
            let tick = t!("reports.verified", sender = "Ann");
            assert!(h.query_by_label(&tick).is_none());
            shot(&mut h, "reports-removed-device", size, dark);
        }
    }
}

#[test]
fn a_report_sealed_before_this_device_is_not_called_unverified() {
    let (mut h, _) = screen(SIZES[0], true, |home| {
        reviewer(home);
        let list = vec![received(true, Err(Unreadable::NotSealedHere))];
        home.server_settings
            .as_mut()
            .unwrap()
            .apply(AdminUpdate::Reports(list));
    });
    if h.query_by_label(t!("reports.page_help")).is_none() {
        h.get_by_label(t!("reports.page")).click();
        h.run_steps(4);
    }
    assert!(h.query_by_label(t!("reports.not_sealed_here")).is_some());
    assert!(h.query_by_label(t!("reports.unverified")).is_none());
    shot(&mut h, "reports-earlier-device", SIZES[0], true);
}

#[test]
fn a_spammers_reports_can_go_in_one_click() {
    let (mut h, mut sent) = screen(SIZES[0], true, |home| {
        reviewer(home);
        let mut second = received(false, Ok(verified()));
        second.message = [41; 32];
        let list = vec![received(true, Ok(verified())), second];
        home.server_settings
            .as_mut()
            .unwrap()
            .apply(AdminUpdate::Reports(list));
    });
    if h.query_by_label(t!("reports.page_help")).is_none() {
        h.get_by_label(t!("reports.page")).click();
        h.run_steps(4);
    }
    let label = t!("reports.dismiss_all", reporter = "Me", count = 2);
    h.query_all(button(&label)).next().unwrap().click();
    h.run_steps(4);
    let asked: Vec<_> = std::iter::from_fn(|| sent.try_recv().ok())
        .filter_map(|command| match command {
            Command::Admin(Admin::DismissFrom(reporter, false)) => Some(reporter),
            _ => None,
        })
        .collect();
    assert_eq!(asked, [ME]);
}

#[test]
fn dismiss_all_counts_only_what_the_server_will_close() {
    for owner in [false, true] {
        let (h, _) = screen(SIZES[0], true, |home| {
            reviewer(home);
            home.servers[0].dir.perms.owner = owner;
            // Two reports by Ann about me.
            let about_me = |n: u8| ReceivedReport {
                id: [n; 32],
                message: [n; 32],
                reporter: ANN,
                sender: ME,
                ..received(true, Ok(verified()))
            };
            let list = vec![about_me(60), about_me(61)];
            let settings = home.server_settings.as_mut().unwrap();
            settings.apply(AdminUpdate::Reports(list));
        });
        // Reports about yourself go in bulk only for the owner (round 5 L1).
        let label = t!("reports.dismiss_all", reporter = "Ann", count = 2);
        assert_eq!(h.query_all(button(&label)).next().is_some(), owner);
    }
}

#[test]
fn the_role_page_counts_the_devices_reports_go_to() {
    let (h, _) = screen(SIZES[0], true, |home| {
        let dir = &mut home.servers[0].dir;
        let device = |n: u8| crate::server::Device {
            signing_key: [n; 32],
            name: String::new(),
            fingerprint: String::new(),
        };
        let mut members = dir.members.edit();
        members[0].roles = vec!["owner".into()];
        members[0].devices = (0..3).map(device).collect();
        members[1].devices = (0..9).map(device).collect();
        drop(members);
        assert_eq!(crate::admin::reports::reviewer_devices(dir, None), 3);
        // Members whose role reviews reports count too.
        dir.roles.push(crate::server::Role {
            id: "member".into(),
            name: "Member".into(),
            name_known: true,
            rank: 0,
            permissions: vec!["review_reports".into()],
            custom: false,
            color: None,
            separate: false,
        });
        assert_eq!(crate::admin::reports::reviewer_devices(dir, None), 12);
        // A role's unsaved permissions count as they would be saved (L1).
        let off: &[String] = &[];
        let on = ["review_reports".to_owned()];
        assert_eq!(
            crate::admin::reports::reviewer_devices(dir, Some(("member", off))),
            3
        );
        dir.roles.last_mut().unwrap().permissions.clear();
        assert_eq!(
            crate::admin::reports::reviewer_devices(dir, Some(("member", &on))),
            12
        );
    });
    drop(h);
}
