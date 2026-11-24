use super::*;
use egui_kittest::{
    Harness,
    kittest::{NodeT, Queryable},
};

#[test]
fn server_rail_marks_only_unavailable_servers() {
    for ((width, height), dark) in [(390, 844), (820, 1180), (1100, 700)]
        .into_iter()
        .flat_map(|size| [(size, true), (size, false)])
    {
        let (mut fixture, _sent) = files_harness(dark);
        let mut home = fixture.state_mut().take().unwrap();
        home.view = View::Server(0);
        home.servers[0].dir.members.edit()[1].presence = Presence::Offline;
        let known = KnownServer {
            address: "https://camp.example.org/".parse().unwrap(),
            server_id: [4; 32],
            tls_fingerprint: None,
        };
        let address = known.address.clone();
        let mut camp = new_server(known, "Camp", home.servers[0].commands.clone());
        camp.dir = home.servers[0].dir.clone();
        camp.dir.icon = Some(Picture {
            key: [4; 32],
            frames: vec![(ColorImage::filled([16, 16], ui::ACCENTS[0].base), 0)].into(),
            bytes: Default::default(),
        });
        home.servers.push(camp);
        home.notify
            .arrived(&address, PROJECTS, notify::unix_now() + 1, true);

        let theme = if dark {
            egui::Theme::Dark
        } else {
            egui::Theme::Light
        };
        let mut h = Harness::builder()
            .with_size(Vec2::new(width as f32, height as f32))
            .with_context_setup(ui::install)
            .with_theme(theme)
            .build_ui_state(
                |ui, home| {
                    let colors = ui::colors(ui);
                    Panel::left("servers")
                        .resizable(false)
                        .exact_size(RAIL_WIDTH)
                        .frame(panel_frame(colors.rail))
                        .show(ui, |ui| server_rail(ui, home));
                    let channels = if ui::is_phone(ui.ctx()) {
                        ui.available_width()
                    } else {
                        layout::CHANNELS_WIDTH
                    };
                    Panel::left("channels")
                        .resizable(false)
                        .exact_size(channels)
                        .frame(panel_frame(colors.side))
                        .show(ui, |ui| sidebar(ui, home));
                    CentralPanel::default()
                        .frame(panel_frame(colors.main))
                        .show(ui, |_| {});
                },
                home,
            );
        h.run_steps(3);
        assert_eq!(h.ctx.global_style().visuals.dark_mode, dark);
        let status = t!("home.server_unavailable");
        let description = |h: &Harness<'_, Home>, name| {
            h.get_by_role_and_label(Role::Button, name)
                .accesskit_node()
                .description()
        };
        let has_badge = |h: &Harness<'_, Home>, name| {
            let rect = h.get_by_role_and_label(Role::Button, name).rect();
            h.output().shapes.iter().any(|shape| {
                matches!(&shape.shape, Shape::Text(text) if rect.contains(text.pos) && text.galley.text() == "!")
            })
        };
        assert_eq!(description(&h, "Camp").as_deref(), Some("Camp"));
        assert_eq!(description(&h, "Friends").as_deref(), Some("Friends"));
        assert!(!has_badge(&h, "Camp"));

        let apply = |h: &mut Harness<'_, Home>, state| {
            let ctx = h.ctx.clone();
            h.state_mut()
                .apply(&ctx, Update::Status(address.clone(), state), 0.0);
            h.run_steps(3);
        };
        let capture = |h: &mut Harness<'_, Home>, phase| {
            if let Some(dir) = std::env::var_os("RUSTTALK_RAIL_SCREENSHOTS") {
                let theme = if dark { "dark" } else { "light" };
                h.render()
                    .unwrap()
                    .save(PathBuf::from(dir).join(format!("{theme}-{width}-{phase}.png")))
                    .unwrap();
            }
        };
        apply(&mut h, Status::Online);
        h.get_by_role_and_label(Role::Button, "Camp").hover();
        h.run_steps(6);
        capture(&mut h, "before");
        apply(&mut h, Status::Offline);
        assert_eq!(description(&h, "Camp").as_deref(), Some(status));
        assert_eq!(description(&h, "Friends").as_deref(), Some("Friends"));
        assert!(has_badge(&h, "Camp"));
        assert!(!has_badge(&h, "Friends"));
        assert_eq!(h.state().server_marks(1), (true, 1));
        h.get_by_label(status);
        capture(&mut h, "after");

        h.input_mut().events.push(egui::Event::PointerGone);
        h.get_by_role_and_label(Role::Button, "Camp").focus();
        h.run_steps(3);
        assert!(h.get_by_role_and_label(Role::Button, "Camp").is_focused());
        h.get_by_label(status);
        h.key_press(Key::Enter);
        h.run_steps(3);
        assert!(h.state().view == View::Server(1));
        assert_eq!(h.state().server_marks(1), (true, 1));

        h.get_by_role_and_label(Role::Button, "Camp")
            .click_secondary();
        h.run_steps(3);
        h.get_by_label(t!("home.server_settings"));
        assert!(h.query_by_label(status).is_none());
        h.key_press(Key::Escape);
        h.run_steps(3);
        h.get_by_role_and_label(Role::Button, "Friends").click();
        h.run_steps(3);
        assert!(h.state().view == View::Server(0));
        let badge = h
            .get_by_role_and_label(Role::Button, "Camp")
            .rect()
            .right_top()
            + Vec2::new(-8.0, 8.0);
        touch::finger(&mut h, egui::TouchPhase::Start, badge);
        assert!(h.query_by_label(status).is_none());
        touch::finger(&mut h, egui::TouchPhase::End, badge);
        h.run_steps(3);
        assert!(h.state().view == View::Server(1));
        assert!(h.query_by_label(status).is_none());

        apply(&mut h, Status::Online);
        assert!(h.state().view == View::Server(1));
        assert_eq!(description(&h, "Camp").as_deref(), Some("Camp"));
        assert_eq!(description(&h, "Friends").as_deref(), Some("Friends"));
        assert!(!has_badge(&h, "Camp"));
        apply(&mut h, Status::Stopped("Connection stopped".into()));
        assert_eq!(description(&h, "Camp").as_deref(), Some(status));
        assert!(has_badge(&h, "Camp"));
        assert_eq!(description(&h, "Friends").as_deref(), Some("Friends"));
    }
}
