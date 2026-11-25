//! Channel managers see the list as members do: no categories without a visible channel,
//! unless they ask for them in the server menu.
use super::*;
use egui_kittest::{Harness, kittest::Queryable};

const STAFF: Id = [5; 32];

fn list(width: f32, height: f32, dark: bool) -> Harness<'static, Home> {
    let (mut fixture, _sent) = files_harness(dark);
    let mut home = fixture.state_mut().take().unwrap();
    home.view = View::Server(0);
    let server = &mut home.servers[0];
    server.dir.perms.manage_channels = true;
    let text = server.dir.categories.remove(0);
    server.dir.categories = vec![
        crate::server::Category {
            id: Some([4; 32]),
            ..text
        },
        // Its only channel is for a role this manager lacks.
        crate::server::Category {
            id: Some(STAFF),
            name: "Staff".into(),
            channels: Vec::new(),
        },
    ];
    let theme = if dark {
        egui::Theme::Dark
    } else {
        egui::Theme::Light
    };
    let mut h = Harness::builder()
        .with_size(Vec2::new(width, height))
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
    h
}

fn staff_shown(h: &Harness<'_, Home>) -> bool {
    let add = t!("home.create_channel_in", category = "Staff");
    h.query_by_label(&add).is_some()
}

fn open_menu(h: &mut Harness<'_, Home>) {
    h.get_by_label("Friends, server menu").click();
    h.run_steps(3);
}

fn toggle(h: &mut Harness<'_, Home>) {
    open_menu(h);
    h.get_by_label(t!("home.show_empty_categories")).click();
    h.run_steps(3);
}

#[test]
fn empty_categories_show_only_when_a_manager_asks() {
    for ((width, height), dark) in [(390.0, 844.0), (1100.0, 700.0)]
        .into_iter()
        .flat_map(|size| [(size, true), (size, false)])
    {
        let mut h = list(width, height, dark);
        let capture = |h: &mut Harness<'_, Home>, phase| {
            if let Some(dir) = std::env::var_os("RUSTTALK_EMPTY_CATEGORIES_SCREENSHOTS") {
                let theme = if dark { "dark" } else { "light" };
                let name = format!("{theme}-{width}x{height}-{phase}.png");
                h.render()
                    .unwrap()
                    .save(PathBuf::from(dir).join(name))
                    .unwrap();
            }
        };
        assert!(h.query_by_label_contains("projects").is_some());
        assert!(!staff_shown(&h));
        capture(&mut h, "hidden");
        open_menu(&mut h);
        capture(&mut h, "menu");
        h.key_press(egui::Key::Escape);
        h.run_steps(3);
        toggle(&mut h);
        assert!(h.state().servers[0].show_empty);
        assert!(staff_shown(&h));
        capture(&mut h, "shown");
        toggle(&mut h);
        assert!(!staff_shown(&h));
    }
}
