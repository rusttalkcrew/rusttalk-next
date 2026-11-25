use super::*;
use egui_kittest::{Harness, kittest::Queryable};

const BOB: Id = [8; 32];
const GENERAL: Id = [5; 32];
const ANN_DM: Id = [9; 32];

/// Friends (with Ann, and a DM with her) and Camp (with Bob and #general).
fn two_servers(dark: bool) -> (Home, tokio::sync::mpsc::UnboundedReceiver<Command>) {
    let (mut fixture, sent) = files_harness(dark);
    let mut home = fixture.state_mut().take().unwrap();
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
    camp.dir.dms.clear();
    let mut bob = camp.dir.members[1].clone();
    bob.person = BOB;
    bob.name = Some("Bob".into());
    camp.dir.members = vec![camp.dir.members[0].clone(), bob].into();
    let channel = &mut camp.dir.categories[0].channels[0];
    channel.id = GENERAL;
    channel.name = "general".into();
    home.servers.push(camp);
    home.view = View::Server(0);
    (home, sent)
}

#[test]
fn quick_switcher_opens_people_channels_and_settings() {
    for ((width, height), dark) in [(390, 844), (1100, 700)]
        .into_iter()
        .flat_map(|size| [(size, true), (size, false)])
    {
        let (home, mut sent) = two_servers(dark);
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
                |ui, home: &mut Home| {
                    // As `show` does: the shortcut before anything can take the key.
                    crate::home::switcher::shortcut(ui.ctx(), home);
                    home.layout.phone = ui::is_phone(ui.ctx());
                    let colors = ui::colors(ui);
                    Panel::left("servers")
                        .resizable(false)
                        .exact_size(RAIL_WIDTH)
                        .frame(panel_frame(colors.rail))
                        .show(ui, |ui| server_rail(ui, home));
                    if !home.layout.phone {
                        Panel::left("channels")
                            .resizable(false)
                            .exact_size(layout::CHANNELS_WIDTH)
                            .frame(panel_frame(colors.side))
                            .show(ui, |ui| sidebar(ui, home));
                    }
                    CentralPanel::default()
                        .frame(panel_frame(colors.main))
                        .show(ui, |ui| chat_pane(ui, home, 0, PROJECTS));
                    crate::home::switcher::show(ui.ctx(), home);
                },
                home,
            );
        h.run_steps(3);
        let capture = |h: &mut Harness<'_, Home>, phase: &str| {
            h.run_steps(8);
            if let Some(dir) = std::env::var_os("RUSTTALK_SWITCHER_SCREENSHOTS") {
                let theme = if dark { "dark" } else { "light" };
                h.render()
                    .unwrap()
                    .save(PathBuf::from(dir).join(format!("{theme}-{width}-{phase}.png")))
                    .unwrap();
            }
        };
        let open = |h: &mut Harness<'_, Home>| {
            h.key_press_modifiers(Modifiers::COMMAND, Key::K);
            h.run_steps(3);
            assert!(h.state().switcher.open);
        };
        let search = |h: &mut Harness<'_, Home>, text: &str| {
            h.event(egui::Event::Text(text.into()));
            h.run_steps(3);
        };

        // Ctrl+K works while typing a message, and leaves the message alone.
        h.ctx.memory_mut(|m| m.request_focus(composer_id(PROJECTS)));
        h.event(egui::Event::Text("hi".into()));
        h.run_steps(2);
        open(&mut h);
        assert_eq!(h.state().servers[0].chats[&PROJECTS].draft, "hi");
        // With an empty box: people with a DM first, then the other members.
        let ann = h.get_by_role_and_label(Role::Button, "Ann").rect();
        let bob = h.get_by_role_and_label(Role::Button, "Bob").rect();
        assert!(ann.top() < bob.top());
        assert!(h.query_by_label("Me").is_none());
        capture(&mut h, "empty");

        // A channel on another server, by Enter: the server switches.
        search(&mut h, "gen");
        h.key_press(Key::Enter);
        h.run_steps(3);
        assert!(!h.state().switcher.open);
        assert!(h.state().view == View::Server(1));
        assert_eq!(h.state().servers[1].open, Some(GENERAL));

        // Reopened: the pick is the first recent one. A member without a DM, by
        // arrows and Enter, starts one on the server they share.
        open(&mut h);
        h.get_by_label(&t!("switcher.recent").to_uppercase());
        capture(&mut h, "recent");
        search(&mut h, "bo");
        h.key_press(Key::ArrowDown);
        h.key_press(Key::ArrowUp);
        h.key_press(Key::Enter);
        h.run_steps(3);
        let mut started = false;
        while let Ok(command) = sent.try_recv() {
            started |= matches!(command, Command::OpenDm(person) if person == BOB);
        }
        assert!(started);

        // A DM that exists opens at once, by a click.
        open(&mut h);
        search(&mut h, "ann");
        capture(&mut h, "search");
        h.get_by_role_and_label(Role::Button, "Ann").click();
        h.run_steps(3);
        assert!(h.state().view == View::Direct(Some(ANN)));

        // A main option opens its Settings page.
        open(&mut h);
        search(&mut h, t!("settings.language"));
        h.key_press(Key::Enter);
        h.run_steps(3);
        assert!(h.state().settings == Some(Tab::Appearance));
        h.state_mut().settings = None;

        // Escape closes; so does Ctrl+K again.
        open(&mut h);
        h.key_press(Key::Escape);
        h.run_steps(3);
        assert!(!h.state().switcher.open);
        open(&mut h);
        h.key_press_modifiers(Modifiers::COMMAND, Key::K);
        h.run_steps(3);
        assert!(!h.state().switcher.open);

        // Phones open it from the search button.
        if width < 600 {
            capture(&mut h, "button");
            h.get_by_role_and_label(Role::Button, t!("switcher.title"))
                .click();
            h.run_steps(3);
            assert!(h.state().switcher.open);
        }
    }
}
