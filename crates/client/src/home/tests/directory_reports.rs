//! The server directory: "Report" on a listing sends a reason and a note, and the owner
//! of a server the directory removed sees why and whom to ask.

use super::*;
use crate::net::{AdminUpdate, Delisted, PublicListing};
use egui_kittest::{
    Harness,
    kittest::{NodeT, Queryable, by},
};

fn screen(size: Vec2, dark: bool, set: impl FnOnce(&mut Home)) -> Harness<'static, Option<Screen>> {
    let (mut files, _sent) = files_harness(dark);
    let mut home = files.state_mut().take().unwrap();
    home.view = View::Server(0);
    home.servers[0].open = Some(PROJECTS);
    // Never a search on the real directory from a test, and no report leaves: an http://
    // directory is refused before anything is sent, so only the updates below arrive.
    home.find.opened = false;
    let mut settings = (*home.voice.audio.snapshot()).clone();
    settings.index_url = "http://127.0.0.1:9/".into();
    home.voice.audio.set_settings(settings);
    set(&mut home);
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
    harness
}

fn home_mut<'h>(h: &'h mut Harness<'_, Option<Screen>>) -> &'h mut Home {
    &mut h.state_mut().as_mut().unwrap().0
}

fn listed() -> crate::net::Listed {
    crate::net::Listed {
        server: KnownServer {
            address: "https://clover.example.org/".parse().unwrap(),
            server_id: [7; 32],
            tls_fingerprint: None,
        },
        name: "Clover Club".into(),
        description: "Board games and voice chat every Friday.".into(),
        members: 12,
        open: true,
    }
}

fn shot(h: &mut Harness<'static, Option<Screen>>, name: &str, size: Vec2, dark: bool) {
    if let Ok(dir) = std::env::var("RUSTTALK_TEST_SCREENSHOTS") {
        let theme = if dark { "dark" } else { "light" };
        h.render()
            .unwrap()
            .save(format!("{dir}/{name}-{}x{}-{theme}.png", size.x, size.y))
            .unwrap();
    }
}

fn apply(h: &mut Harness<'static, Option<Screen>>, update: crate::net::Update) {
    let ctx = h.ctx.clone();
    home_mut(h).apply(&ctx, update, 0.0);
    h.run_steps(4);
}

#[test]
fn reporting_a_listing_needs_a_reason_and_says_when_it_was_sent() {
    for dark in [true, false] {
        for size in [Vec2::new(390.0, 844.0), Vec2::new(1100.0, 700.0)] {
            let mut h = screen(size, dark, |home| {
                home.view = View::Find;
                home.layout.list = false;
                home.find.page = 1;
                home.find.results = vec![listed()];
            });
            shot(&mut h, "find-report-button", size, dark);
            h.get_by_label(&t!("find.report_named", server = "Clover Club"))
                .click();
            h.run_steps(4);
            assert!(home_mut(&mut h).find.report.is_some());
            let send = || by().role(Role::Button).label(t!("find.report_send"));
            assert!(h.get(send()).accesskit_node().is_disabled());
            h.get(by().role(Role::RadioButton).label(t!("find.reason_scam")))
                .click();
            h.run_steps(4);
            assert!(!h.get(send()).accesskit_node().is_disabled());
            shot(&mut h, "find-report-dialog", size, dark);
            h.get(send()).click();
            h.run_steps(4);
            assert!(h.query_by_label(t!("home.use_https")).is_some());
            let report = home_mut(&mut h).find.report.as_mut().unwrap();
            assert!(!report.sending, "nothing was sent");
            assert_eq!(report.reason, Some(crate::net::ReportReason::Scam));

            // A refusal stays in the dialog; a sent report closes it with thanks.
            report.sending = true;
            let limit = t!("find.report_limit");
            apply(&mut h, crate::net::Update::Reported(Err(limit.into())));
            shot(&mut h, "find-report-error", size, dark);
            assert!(h.query_by_label(limit).is_some());
            assert!(!home_mut(&mut h).find.report.as_ref().unwrap().sending);
            apply(&mut h, crate::net::Update::Reported(Ok(())));
            assert!(home_mut(&mut h).find.report.is_none());
            assert!(h.query_by_label(t!("find.report_sent")).is_some());
            shot(&mut h, "find-report-sent", size, dark);
        }
    }
}

#[test]
fn owners_see_why_the_directory_removed_their_server() {
    let listing = || PublicListing {
        name: "Clover Club".into(),
        description: "Board games and voice chat every Friday.".into(),
        address: "https://clover.example.org/".into(),
        index_url: crate::official::DIRECTORY.into(),
        index_fingerprint: String::new(),
    };
    for dark in [true, false] {
        for size in [Vec2::new(390.0, 844.0), Vec2::new(1100.0, 700.0)] {
            let removed = |delisted| {
                move |home: &mut Home| {
                    home.servers[0].dir.perms.owner = true;
                    crate::admin::open(home, 0, crate::admin::Tab::Listing);
                    let settings = home.server_settings.as_mut().unwrap();
                    settings.apply(AdminUpdate::Listing(true, Some(listing()), delisted));
                }
            };
            let open_page = |h: &mut Harness<'static, Option<Screen>>| {
                if h.query_by_label(t!("admin.directory_rules")).is_none() {
                    // Phones may start on the page list.
                    h.get_by_label(t!("admin.public_listing")).click();
                    h.run_steps(4);
                }
            };
            let mut h = screen(size, dark, removed(None));
            open_page(&mut h);
            assert!(h.query_by_label(t!("admin.directory_rules")).is_some());
            assert!(h.query_by_label(t!("admin.delisted_title")).is_none());

            let mut h = screen(
                size,
                dark,
                removed(Some(Delisted {
                    reason: Some("Spam links in the description".into()),
                    contact: Some("directory@example.org".into()),
                })),
            );
            open_page(&mut h);
            assert!(h.query_by_label(t!("admin.delisted_title")).is_some());
            let reason = t!(
                "admin.delisted_reason",
                reason = "Spam links in the description"
            );
            assert!(h.query_by_label(&reason).is_some());
            let contact = t!("admin.delisted_contact", contact = "directory@example.org");
            assert!(h.query_by_label(&contact).is_some());
            shot(&mut h, "listing-removed", size, dark);

            let mut h = screen(size, dark, removed(Some(Delisted::default())));
            open_page(&mut h);
            assert!(h.query_by_label(t!("admin.delisted_no_contact")).is_some());
        }
    }
}
