use super::*;
use crate::server::{Call, CallState, Peer, Screen, Stats};
use egui_kittest::{Harness, kittest::Queryable};

#[test]
fn stream_cards_keep_people_opt_in_and_preview_follows_focus() {
    for dark in [true, false] {
        for (width, height) in [(390.0, 844.0), (820.0, 1180.0), (1100.0, 700.0)] {
            let (mut fixture, mut sent) = files_harness(dark);
            let mut home = fixture.state_mut().take().unwrap();
            home.voice.call = Some(Call {
                server: home.servers[0].address.clone(),
                channel: PROJECTS,
                state: CallState::Connected,
                stats: Stats::default(),
                can_talk: true,
                can_share_video: true,
            });
            let peer = |person, local: bool, count| Peer {
                person,
                connecting: false,
                unknown: false,
                status: Default::default(),
                apps: vec![],
                camera: (!local).then(|| "ann-camera".into()),
                terminal: None,
                screens: vec![Screen {
                    sid: if local { "local" } else { "remote" }.into(),
                    name: "Display".into(),
                    viewers: count,
                    local,
                }],
            };
            home.servers[0]
                .voice
                .insert(PROJECTS, vec![peer(ME, true, 0), peer(ANN, false, 2)]);
            home.voice.sharing = call::Sharing::On;
            let mut h = Harness::builder()
                .with_context_setup(ui::install)
                .with_size(Vec2::new(width, height))
                .build_ui_state(
                    |ui, home| {
                        call::frame(ui.ctx(), home);
                        call::stage(ui, home, 0, PROJECTS);
                    },
                    home,
                );
            h.ctx.set_theme(if dark {
                egui::Theme::Dark
            } else {
                egui::Theme::Light
            });
            h.input_mut().focused = true;
            h.run_steps(4);
            assert!(h.state().voice.video.preview.is_on());
            assert!(h.state().voice.watching.is_none());
            assert!(h.query_by_label("Streaming · 0 watching").is_some());
            assert!(h.query_by_label("Streaming · 2 watching").is_some());
            if let Ok(dir) = std::env::var("RUSTTALK_STREAM_SCREENSHOTS") {
                std::fs::create_dir_all(&dir).unwrap();
                h.render()
                    .unwrap()
                    .save(format!("{dir}/stream-{width}-{dark}.png"))
                    .unwrap();
            }
            h.input_mut().focused = false;
            h.run_steps(2);
            assert!(!h.state().voice.video.preview.is_on());
            assert!(h.state().voice.sharing == call::Sharing::On);
            h.input_mut().focused = true;
            h.run_steps(2);
            assert!(h.state().voice.video.preview.is_on());
            h.input_mut()
                .viewports
                .get_mut(&egui::ViewportId::ROOT)
                .unwrap()
                .minimized = Some(true);
            h.run_steps(2);
            assert!(!h.state().voice.video.preview.is_on());
            h.input_mut()
                .viewports
                .get_mut(&egui::ViewportId::ROOT)
                .unwrap()
                .minimized = Some(false);
            h.run_steps(2);
            assert!(h.state().voice.video.preview.is_on());
            h.state_mut().servers[0].voice.get_mut(&PROJECTS).unwrap()[1].screens[0].viewers = 1;
            h.run_steps(2);
            assert!(h.query_by_label("Streaming · 1 watching").is_some());
            h.get_by_label("Watch Ann").click();
            h.run_steps(3);
            assert_eq!(h.state().voice.watching.as_ref().unwrap().sid, "remote");
            let mut camera_changes = || {
                std::iter::from_fn(|| sent.try_recv().ok())
                    .filter_map(|command| match command {
                        Command::Watch(sid, enabled) if sid == "ann-camera" => Some(enabled),
                        _ => None,
                    })
                    .collect::<Vec<_>>()
            };
            assert!(camera_changes().is_empty());
            let mut screens = std::mem::take(
                &mut h.state_mut().servers[0].voice.get_mut(&PROJECTS).unwrap()[1].screens,
            );

            let address = h.state().servers[0].address.clone();
            let ctx = h.ctx.clone();
            h.state_mut().apply(
                &ctx,
                Update::Call(address.clone(), PROJECTS, Some(CallState::Reconnecting)),
                0.0,
            );
            h.run_steps(2);
            assert_eq!(h.state().voice.watching.as_ref().unwrap().sid, "remote");
            assert!(h.query_by_label("Streaming · 1 watching").is_none());
            assert!(h.query_by_label("Ann").is_some());
            assert_eq!(camera_changes(), [true]);
            screens[0].sid = "replacement".into();
            h.state_mut().apply(
                &ctx,
                Update::VideoReplaced(
                    address.clone(),
                    PROJECTS,
                    "remote".into(),
                    "replacement".into(),
                ),
                0.0,
            );
            h.state_mut().servers[0].voice.get_mut(&PROJECTS).unwrap()[1].screens = screens;
            h.state_mut().apply(
                &ctx,
                Update::Call(address.clone(), PROJECTS, Some(CallState::Connected)),
                0.0,
            );
            h.run_steps(3);
            assert_eq!(camera_changes(), [false]);
            assert_eq!(
                h.state().voice.watching.as_ref().unwrap().sid,
                "replacement"
            );
            if let Ok(dir) = std::env::var("RUSTTALK_STREAM_SCREENSHOTS") {
                h.render()
                    .unwrap()
                    .save(format!("{dir}/recovered-{width}-{dark}.png"))
                    .unwrap();
            }
            // Explicit hangup wins over a late replacement update.
            call::leave(h.state_mut());
            h.state_mut().apply(
                &ctx,
                Update::VideoReplaced(address, PROJECTS, "replacement".into(), "late".into()),
                0.0,
            );
            assert!(h.state().voice.watching.is_none());
        }
    }
}

#[test]
fn normal_stop_share_exits_fullscreen_and_camera_remap_avoids_spurious_unwatch() {
    let (mut fixture, mut sent) = files_harness(true);
    let mut home = fixture.state_mut().take().unwrap();
    let address = home.servers[0].address.clone();
    home.voice.call = Some(Call {
        server: address.clone(),
        channel: PROJECTS,
        state: CallState::Connected,
        stats: Stats::default(),
        can_talk: true,
        can_share_video: true,
    });
    home.servers[0].voice.insert(
        PROJECTS,
        vec![
            Peer {
                person: ME,
                connecting: false,
                unknown: false,
                status: Default::default(),
                apps: vec![],
                camera: None,
                terminal: None,
                screens: vec![],
            },
            Peer {
                person: ANN,
                connecting: false,
                unknown: false,
                status: Default::default(),
                apps: vec![],
                camera: Some("ann-cam-1".into()),
                terminal: None,
                screens: vec![],
            },
        ],
    );
    let mut h = Harness::builder()
        .with_context_setup(ui::install)
        .with_size(Vec2::new(1100.0, 700.0))
        .build_ui_state(
            |ui, home| {
                call::frame(ui.ctx(), home);
                if home.voice.stage.full.is_some() {
                    call::full_view(ui, home);
                } else {
                    call::stage(ui, home, 0, PROJECTS);
                }
            },
            home,
        );
    h.run_steps(3);
    // Remote camera is watched while visible on stage.
    let initial: Vec<_> = std::iter::from_fn(|| sent.try_recv().ok()).collect();
    assert!(
        initial
            .iter()
            .any(|cmd| matches!(cmd, Command::Watch(sid, true) if sid == "ann-cam-1"))
    );

    // Camera track ID changes on restart: replace_video remaps Voice::cameras so the next frame
    // does not send Watch("ann-cam-1", false) or re-subscribe redundantly.
    let ctx = h.ctx.clone();
    h.state_mut().apply(
        &ctx,
        Update::VideoReplaced(
            address.clone(),
            PROJECTS,
            "ann-cam-1".into(),
            "ann-cam-2".into(),
        ),
        0.0,
    );
    h.state_mut().servers[0].voice.get_mut(&PROJECTS).unwrap()[1].camera = Some("ann-cam-2".into());
    h.run_steps(3);
    let cam_cmds: Vec<_> = std::iter::from_fn(|| sent.try_recv().ok())
        .filter_map(|cmd| match cmd {
            Command::Watch(sid, on) if sid.starts_with("ann-cam-") => Some((sid, on)),
            _ => None,
        })
        .collect();
    assert!(
        cam_cmds.is_empty(),
        "expected no camera watch churn after remap, got {cam_cmds:?}"
    );

    // Ann shares a screen; watch it, enter presentation mode and fullscreen, then Ann stops normally.
    h.state_mut().servers[0].voice.get_mut(&PROJECTS).unwrap()[1].screens = vec![Screen {
        sid: "ann-scr-1".into(),
        name: "Display".into(),
        viewers: 1,
        local: false,
    }];
    h.run_steps(2);
    h.get_by_label("Watch Ann").click();
    h.run_steps(2);
    assert_eq!(h.state().voice.watching.as_ref().unwrap().sid, "ann-scr-1");
    h.state_mut()
        .voice
        .stage
        .set_presenting_and_full_screen("ann-scr-1");
    h.run_steps(2);
    assert!(h.state().voice.stage.full.is_some());

    // Healthy call: presenter stops sharing -> watching, fullscreen, and presentation clear immediately.
    h.state_mut().servers[0].voice.get_mut(&PROJECTS).unwrap()[1]
        .screens
        .clear();
    h.run_steps(2);
    assert!(h.state().voice.watching.is_none());
    assert!(h.state().voice.stage.full.is_none());
    assert!(!h.state().voice.stage.is_presenting());

    // A later screen share from the same presenter stays opt-in.
    h.state_mut().servers[0].voice.get_mut(&PROJECTS).unwrap()[1].screens = vec![Screen {
        sid: "ann-scr-2".into(),
        name: "Display".into(),
        viewers: 0,
        local: false,
    }];
    h.run_steps(2);
    assert!(h.state().voice.watching.is_none());
    assert!(h.state().voice.stage.full.is_none());

    // Post-recovery grace window keeps watching briefly after Reconnecting -> Connected until
    // the remote sharer republishes, then clears normally on a subsequent stop-share.
    h.get_by_label("Watch Ann").click();
    h.run_steps(2);
    assert_eq!(h.state().voice.watching.as_ref().unwrap().sid, "ann-scr-2");
    h.state_mut()
        .voice
        .stage
        .set_presenting_and_full_screen("ann-scr-2");
    h.state_mut().apply(
        &ctx,
        Update::Call(address.clone(), PROJECTS, Some(CallState::Reconnecting)),
        0.0,
    );
    h.state_mut().servers[0].voice.get_mut(&PROJECTS).unwrap()[1]
        .screens
        .clear();
    h.run_steps(2);
    assert!(h.state().voice.stage.full.is_some());
    // Local call reconnects before Ann finishes republishing.
    h.state_mut().apply(
        &ctx,
        Update::Call(address.clone(), PROJECTS, Some(CallState::Connected)),
        0.0,
    );
    h.run_steps(2);
    assert_eq!(h.state().voice.watching.as_ref().unwrap().sid, "ann-scr-2");
    assert!(h.state().voice.stage.full.is_some());
    // Ann republishes with a new track SID, then later stops sharing normally.
    h.state_mut().apply(
        &ctx,
        Update::VideoReplaced(address, PROJECTS, "ann-scr-2".into(), "ann-scr-3".into()),
        0.0,
    );
    h.state_mut().servers[0].voice.get_mut(&PROJECTS).unwrap()[1].screens = vec![Screen {
        sid: "ann-scr-3".into(),
        name: "Display".into(),
        viewers: 1,
        local: false,
    }];
    h.run_steps(2);
    assert_eq!(h.state().voice.watching.as_ref().unwrap().sid, "ann-scr-3");
    assert!(h.state().voice.stage.full.is_some());
    h.state_mut().servers[0].voice.get_mut(&PROJECTS).unwrap()[1]
        .screens
        .clear();
    h.run_steps(2);
    assert!(h.state().voice.watching.is_none());
    assert!(h.state().voice.stage.full.is_none());
    assert!(!h.state().voice.stage.is_presenting());
}
