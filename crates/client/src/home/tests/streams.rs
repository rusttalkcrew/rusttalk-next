use super::*;
use crate::server::{Call, CallState, Peer, Screen, Stats};
use egui_kittest::{Harness, kittest::Queryable};

#[test]
fn stream_cards_keep_people_opt_in_and_preview_follows_focus() {
    for dark in [true, false] {
        for (width, height) in [(390.0, 844.0), (820.0, 1180.0), (1100.0, 700.0)] {
            let (mut fixture, _sent) = files_harness(dark);
            let mut home = fixture.state_mut().take().unwrap();
            home.voice.call = Some(Call {
                server: home.servers[0].address.clone(),
                channel: PROJECTS,
                state: CallState::Connected,
                stats: Stats::default(),
                can_talk: true,
                can_share_video: true,
            });
            let peer = |person, local, count| Peer {
                person,
                connecting: false,
                unknown: false,
                status: Default::default(),
                apps: vec![],
                camera: None,
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
                        home.voice.update_preview(ui.ctx());
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
            h.state_mut().servers[0].voice.get_mut(&PROJECTS).unwrap()[1]
                .screens
                .clear();
            h.run_steps(2);
            assert!(h.query_by_label("Streaming · 1 watching").is_none());
            assert!(h.query_by_label("Ann").is_some());
        }
    }
}
