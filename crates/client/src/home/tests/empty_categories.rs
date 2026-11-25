//! Members see no categories without a visible channel; people who manage channels see
//! every category, so an empty one doesn't vanish before they can fill or delete it.
use super::*;
use egui_kittest::{Harness, kittest::Queryable};

const STAFF: Id = [5; 32];

fn list(width: f32, height: f32, dark: bool, manager: bool) -> Harness<'static, Home> {
    let (mut fixture, _sent) = files_harness(dark);
    let mut home = fixture.state_mut().take().unwrap();
    home.view = View::Server(0);
    let server = &mut home.servers[0];
    server.dir.perms.manage_channels = manager;
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

#[test]
fn managers_see_empty_categories_and_members_do_not() {
    for ((width, height), dark) in [(390.0, 844.0), (1100.0, 700.0)]
        .into_iter()
        .flat_map(|size| [(size, true), (size, false)])
    {
        let mut h = list(width, height, dark, true);
        assert!(h.query_by_label_contains("projects").is_some());
        assert!(staff_shown(&h), "a manager keeps the empty category");
        if let Some(dir) = std::env::var_os("RUSTTALK_EMPTY_CATEGORIES_SCREENSHOTS") {
            let theme = if dark { "dark" } else { "light" };
            let name = format!("{theme}-{width}x{height}-manager.png");
            h.render()
                .unwrap()
                .save(PathBuf::from(dir).join(name))
                .unwrap();
        }
        let h = list(width, height, dark, false);
        assert!(h.query_by_label_contains("projects").is_some());
        let header = h.query_by_label_contains("Staff").is_some();
        assert!(!header, "a member doesn't see a category without channels");
    }
}
