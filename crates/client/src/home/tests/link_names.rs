//! The join page never saves an invite or another link as a name (#658).

use super::*;
use egui_kittest::{
    Harness,
    kittest::{NodeT, Queryable, by},
};

fn join_page(name: &str, dark: bool) -> Harness<'static, Home> {
    let (mut fixture, _sent) = files_harness(dark);
    let mut home = fixture.state_mut().take().unwrap();
    home.join = Join::Preview {
        preview: Box::new(crate::net::Preview {
            server: KnownServer {
                address: "https://games.example.test/".parse().unwrap(),
                server_id: [4; 32],
                tls_fingerprint: None,
            },
            invite: Some([5; 16]),
            name: Some("Game Night".into()),
            known: false,
            open: false,
            member: false,
        }),
        name: name.into(),
    };
    let mut h = Harness::builder()
        .with_size(Vec2::new(1100.0, 700.0))
        .with_context_setup(ui::install)
        .with_theme(if dark {
            egui::Theme::Dark
        } else {
            egui::Theme::Light
        })
        .build_ui_state(
            |ui, home: &mut Home| {
                CentralPanel::default().show(ui, |ui| join(ui, home));
            },
            home,
        );
    h.run_steps(3);
    h
}

fn join_enabled(h: &Harness<'static, Home>) -> bool {
    !h.get(
        by().role(egui::accesskit::Role::Button)
            .label(t!("common.join")),
    )
    .accesskit_node()
    .is_disabled()
}

#[test]
fn join_page_refuses_an_invite_or_link_as_a_name() {
    let invite = "rusttalk://other.example.test/i/AAAAAAAA#BBBBBBBB";
    for dark in [true, false] {
        let mut h = join_page(invite, dark);
        if let Some(dir) = std::env::var_os("RUSTTALK_LINK_NAME_SCREENSHOTS") {
            let theme = if dark { "dark" } else { "light" };
            h.render()
                .unwrap()
                .save(PathBuf::from(dir).join(format!("join-{theme}-1100x700.png")))
                .unwrap();
        }
        h.get_by_label(t!("names.invite_in_name"));
        assert!(!join_enabled(&h));
        // Clicking anyway joins nothing.
        h.get_by_label(t!("common.join")).click();
        h.run_steps(2);
        assert!(matches!(h.state().join, Join::Preview { .. }));
    }
    let h = join_page("see example.test/promo", true);
    h.get_by_label(t!("names.no_links"));
    assert!(!join_enabled(&h));
    let h = join_page("J. Smith", true);
    assert!(h.query_by_label(t!("names.no_links")).is_none());
    assert!(join_enabled(&h));
}
