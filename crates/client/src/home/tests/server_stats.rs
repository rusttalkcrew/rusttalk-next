//! Server statistics: Server settings > Statistics shows the server's counts and sizes, asks
//! again while open, and only shows for people allowed to view them.

use super::*;
use crate::net::{Admin, AdminUpdate};
use egui_kittest::{
    Harness,
    kittest::{Queryable, by},
};
use rusttalk_core::auth::{
    ServerStats, StatsDirection, StatsHistory, StatsPeak, StatsPoint, StatsStorage, StatsTraffic,
    StatsVoice,
};

type Sent = tokio::sync::mpsc::UnboundedReceiver<Command>;

fn stats() -> ServerStats {
    ServerStats {
        version: "9.8.7".into(),
        started_at: 1_700_000_000,
        uptime_secs: 2 * 86_400 + 3 * 3600 + 4 * 60,
        connected: 12,
        peak: StatsPeak {
            count: 17,
            at: 1_700_003_600,
        },
        members: 40,
        logins: 321,
        messages: 4567,
        traffic: StatsTraffic {
            control: StatsDirection {
                received: 3 * 1024 * 1024,
                sent: 5 * 1024 * 1024,
                received_per_sec: 2048,
                sent_per_sec: 4096,
            },
            media: Some(StatsDirection {
                received: 20 * 1024 * 1024,
                sent: 60 * 1024 * 1024,
                received_per_sec: 100 * 1024,
                sent_per_sec: 300 * 1024,
            }),
        },
        storage: StatsStorage {
            database: 2 * 1024 * 1024,
            files: 6 * 1024 * 1024,
        },
        memory: Some(48 * 1024 * 1024),
        voice: Some(StatsVoice {
            people: 5,
            channels: 2,
            audio: 4,
            camera: 2,
            screen: 1,
        }),
    }
}

/// Server settings, on Statistics when `allowed`.
fn screen(size: Vec2, dark: bool, allowed: bool) -> (Harness<'static, Option<Screen>>, Sent) {
    let (mut files, sent) = files_harness(dark);
    let mut home = files.state_mut().take().unwrap();
    home.view = View::Server(0);
    home.servers[0].open = Some(PROJECTS);
    home.find.opened = false;
    home.servers[0].dir.perms.manage_server = true;
    home.servers[0].dir.perms.stats = allowed;
    let tab = if allowed {
        crate::admin::Tab::Stats
    } else {
        crate::admin::Tab::Overview
    };
    crate::admin::open(&mut home, 0, tab);
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
    if allowed && harness.query_by_label(t!("admin.stats_help")).is_none() {
        // Phones may start on the page list.
        if let Some(page) = harness.query_all_by_label(t!("admin.stats")).next() {
            page.click();
        }
        harness.run_steps(4);
    }
    (harness, sent)
}

fn apply(h: &mut Harness<'static, Option<Screen>>, update: Result<ServerStats, String>) {
    let (home, _, _) = h.state_mut().as_mut().unwrap();
    home.server_settings
        .as_mut()
        .unwrap()
        .apply(AdminUpdate::Stats(update));
    h.run_steps(4);
}

fn asked(sent: &mut Sent) -> usize {
    std::iter::from_fn(|| sent.try_recv().ok())
        .filter(|command| matches!(command, Command::Admin(Admin::LoadStats)))
        .count()
}

#[test]
fn people_allowed_see_the_numbers_and_they_refresh() {
    let screenshots = std::env::var("RUSTTALK_TEST_SCREENSHOTS").ok();
    for dark in [true, false] {
        for size in [
            Vec2::new(390.0, 844.0),
            Vec2::new(820.0, 1180.0),
            Vec2::new(1100.0, 700.0),
        ] {
            let (mut h, mut sent) = screen(size, dark, true);
            // Opening the page asks once, not every frame.
            assert_eq!(asked(&mut sent), 1);
            h.run_steps(4);
            assert_eq!(asked(&mut sent), 0);
            apply(&mut h, Ok(stats()));
            let connected = t!("admin.stats_connected").to_uppercase();
            for text in [
                connected.as_str(),
                "12",
                "321",
                "4567",
                // Incoming adds the API's and LiveKit's traffic.
                "102 KB/s",
                "23.0 MB since start",
                "8.0 MB",
                "in 2 channels",
                "Cameras: 2 · Screens: 1",
                "Voice and video 60.0 MB",
                "Control and chat 5.0 MB",
                "2 d 3 h 4 min",
                "Version 9.8.7",
                "48.0 MB",
                t!("admin.stats_media_scope"),
            ] {
                assert!(
                    h.query_all_by_label_contains(text).next().is_some(),
                    "{text} missing"
                );
            }
            if let Some(dir) = &screenshots {
                let theme = if dark { "dark" } else { "light" };
                h.render()
                    .unwrap()
                    .save(format!(
                        "{dir}/server-stats-{}x{}-{theme}.png",
                        size.x, size.y
                    ))
                    .unwrap();
            }
            // The traffic bars, reached from the page's section list.
            if size.x > 1000.0 {
                if let Some(link) = h.query_all_by_label(t!("admin.stats_by_kind")).next() {
                    link.click();
                }
                h.run_steps(8);
                if let Some(dir) = &screenshots {
                    let theme = if dark { "dark" } else { "light" };
                    h.render()
                        .unwrap()
                        .save(format!("{dir}/server-stats-traffic-{theme}.png"))
                        .unwrap();
                }
            }
            // A failed refresh keeps the last numbers and says what went wrong.
            apply(&mut h, Err("Server unreachable".into()));
            assert!(h.query_by_label_contains("Server unreachable").is_some());
            assert!(h.query_all_by_label_contains("4567").next().is_some());
        }
    }
}

fn history(range: i64) -> StatsHistory {
    let step = range / 60;
    let end = crate::notify::unix_now();
    let points = (1..=60)
        .map(|i: i64| StatsPoint {
            at: end - range + i * step,
            received_per_sec: (i != 1).then_some((40_000 + (i * 7919) % 30_000) as u64),
            sent_per_sec: (i != 1).then_some((90_000 + (i * 104_729) % 50_000) as u64),
            connected: (8 + i % 5) as u64,
            // LiveKit didn't answer for a while.
            voice: (!(20..25).contains(&i)).then_some((3 + i % 3) as u64),
        })
        .collect();
    StatsHistory {
        range,
        step,
        points,
    }
}

fn histories_asked(sent: &mut Sent) -> Vec<i64> {
    std::iter::from_fn(|| sent.try_recv().ok())
        .filter_map(|command| match command {
            Command::Admin(Admin::LoadStatsHistory(range)) => Some(range),
            _ => None,
        })
        .collect()
}

#[test]
fn history_shows_and_follows_the_picked_range() {
    let screenshots = std::env::var("RUSTTALK_TEST_SCREENSHOTS").ok();
    for dark in [true, false] {
        let (mut h, mut sent) = screen(Vec2::new(1100.0, 700.0), dark, true);
        apply(&mut h, Ok(stats()));
        h.run_steps(4);
        // An hour first, asked once the numbers are in.
        assert_eq!(histories_asked(&mut sent), [3600]);
        {
            let (home, _, _) = h.state_mut().as_mut().unwrap();
            let settings = home.server_settings.as_mut().unwrap();
            settings.apply(AdminUpdate::StatsHistory(Ok(history(3600))));
        }
        h.run_steps(4);
        if let Some(link) = h.query_all_by_label(t!("admin.stats_history")).next() {
            link.click();
        }
        h.run_steps(8);
        let traffic = t!("admin.stats_traffic_chart").to_uppercase();
        assert!(h.query_all_by_label_contains(&traffic).next().is_some());
        assert!(
            h.query_by_label_contains(t!("admin.stats_history_help"))
                .is_some()
        );
        if let Some(dir) = &screenshots {
            let theme = if dark { "dark" } else { "light" };
            h.render()
                .unwrap()
                .save(format!("{dir}/server-stats-history-{theme}.png"))
                .unwrap();
        }
        // Another range is asked for at once, and the old lines go.
        let five = by()
            .role(egui::accesskit::Role::RadioButton)
            .label(t!("admin.stats_range_5_min"));
        h.get(five).click();
        h.run_steps(4);
        assert_eq!(histories_asked(&mut sent), [300]);
        // A late answer for the hour is ignored.
        {
            let (home, _, _) = h.state_mut().as_mut().unwrap();
            let settings = home.server_settings.as_mut().unwrap();
            settings.apply(AdminUpdate::StatsHistory(Ok(history(3600))));
        }
        h.run_steps(4);
        assert!(h.query_all_by_label_contains(&traffic).next().is_none());
    }
}

#[test]
fn without_livekit_the_rest_still_shows() {
    let (mut h, _) = screen(Vec2::new(1100.0, 700.0), true, true);
    let mut older = stats();
    (older.voice, older.traffic.media) = (None, None);
    apply(&mut h, Ok(older));
    assert!(
        h.query_by_label_contains(t!("admin.stats_no_media"))
            .is_some()
    );
    assert!(h.query_all_by_label_contains("2 KB/s").next().is_some());
    let in_voice = t!("admin.stats_in_voice").to_uppercase();
    assert!(h.query_all_by_label_contains(&in_voice).next().is_none());
}

#[test]
fn the_page_is_hidden_without_the_permission() {
    let (h, mut sent) = screen(Vec2::new(1100.0, 700.0), true, false);
    assert!(h.query_all_by_label(t!("admin.stats")).next().is_none());
    assert_eq!(asked(&mut sent), 0);
}
