use super::*;
use crate::friends::{AllowDms, Sort};
use egui_kittest::{Harness, kittest::Queryable};

const ANN_DM: Id = [9; 32];

/// Ann, who isn't a friend, wrote first: her DM waits under Message requests.
fn request(dark: bool, size: Vec2) -> Harness<'static, Home> {
    let (mut fixture, _sent) = files_harness(dark);
    let mut home = fixture.state_mut().take().unwrap();
    home.servers[0].dir.dms.push(crate::server::Dm {
        channel: ANN_DM,
        peer: ANN,
    });
    home.threads.entry(ANN).or_default().chat.messages = vec![Msg {
        id: [4; 32],
        person: ANN,
        at: 1_700_000_000,
        body: Body::Text("Hi! Is the bike from the market post still for sale?".into()),
        backfilled: false,
        reply_to: None,
        quoted_author: None,
        edit_of: None,
        files: vec![],
        live: None,
        sticker: None,
        replicas: vec![],
        copy_files: vec![],
    }];
    home.view = View::Direct(Some(ANN));
    let mut h = Harness::builder()
        .with_size(size)
        .with_context_setup(ui::install)
        .with_theme(if dark {
            egui::Theme::Dark
        } else {
            egui::Theme::Light
        })
        .build_ui_state(
            |ui, home: &mut Home| {
                home.layout.phone = ui::is_phone(ui.ctx());
                let colors = ui::colors(ui);
                if !home.layout.phone {
                    Panel::left("channels")
                        .resizable(false)
                        .exact_size(layout::CHANNELS_WIDTH)
                        .frame(panel_frame(colors.side))
                        .show(ui, |ui| sidebar(ui, home));
                }
                CentralPanel::default()
                    .frame(panel_frame(colors.main))
                    .show(ui, |ui| {
                        if let View::Direct(Some(peer)) = home.view {
                            dm_pane(ui, home, peer);
                        }
                    });
            },
            home,
        );
    h.run_steps(6);
    h
}

fn save(h: &mut Harness<'_, Home>, name: &str, dark: bool, size: Vec2) {
    if let Some(dir) = std::env::var_os("RUSTTALK_REQUESTS_SCREENSHOTS") {
        let theme = if dark { "dark" } else { "light" };
        h.render()
            .unwrap()
            .save(PathBuf::from(dir).join(format!(
                "{name}-{theme}-{}x{}.png",
                size.x as u32, size.y as u32
            )))
            .unwrap();
    }
}

#[test]
fn message_requests_accept_delete_and_block() {
    for dark in [true, false] {
        for size in [Vec2::new(390., 844.), Vec2::new(1100., 700.)] {
            let mut h = request(dark, size);
            assert_eq!(h.state().dm_sort(&ANN), Sort::Request);
            if size.x > 500. {
                assert!(h.query_by_label("Message requests (1)").is_some());
            }
            assert!(h.query_by_label(t!("friends.delete_request")).is_some());
            save(&mut h, "request", dark, size);

            h.get_by_label(t!("friends.accept")).click();
            h.run_steps(3);
            assert_eq!(h.state().dm_sort(&ANN), Sort::Shown);
            assert!(h.query_by_label(t!("friends.delete_request")).is_none());
            assert!(h.query_by_label("Message requests (1)").is_none());

            let mut h = request(dark, size);
            h.get_by_label(t!("friends.delete_request")).click();
            h.run_steps(3);
            assert!(h.state().view == View::Direct(None));
            assert_eq!(h.state().dm_sort(&ANN), Sort::Hidden);

            let mut h = request(dark, size);
            h.get_by_label(t!("friends.block")).click();
            h.run_steps(3);
            assert_eq!(h.state().friends.relation(&ANN), Relation::Blocked);
            assert_eq!(h.state().dm_sort(&ANN), Sort::Hidden);
        }
    }
}

/// "Friends only" hides her DM altogether; "Everyone" lets it straight in.
#[test]
fn who_can_dm_setting_sorts_the_request() {
    for dark in [true, false] {
        for size in [Vec2::new(390., 844.), Vec2::new(1100., 700.)] {
            let (mut fixture, _sent) = files_harness(dark);
            let home = fixture.state_mut().take().unwrap();
            let mut h = Harness::builder()
                .with_size(size)
                .with_context_setup(ui::install)
                .with_theme(if dark {
                    egui::Theme::Dark
                } else {
                    egui::Theme::Light
                })
                .build_ui_state(
                    |ui, home: &mut Home| {
                        CentralPanel::default().show(ui, |ui| allow_dms(ui, home));
                    },
                    home,
                );
            h.run_steps(3);
            assert_eq!(
                h.state().voice.audio.settings().allow_dms,
                AllowDms::Requests
            );
            save(&mut h, "setting", dark, size);
            h.get_by_label(t!("home.allow_dms_friends")).click();
            h.run_steps(2);
            let settings = h.state().voice.audio.settings();
            assert_eq!(settings.allow_dms, AllowDms::Friends);
            assert!(settings.friends_only);
            h.get_by_label(t!("home.allow_dms_everyone")).click();
            h.run_steps(2);
            assert!(!h.state().voice.audio.settings().friends_only);
        }
    }
}
