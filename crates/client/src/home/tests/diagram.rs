//! A diagram drawn together in a call (#336): started from the call's "…" menu, it
//! takes the stage for everyone, others join on their own and ask to edit, and the
//! host allows each. Set RUSTTALK_DIAGRAM_SCREENSHOTS to save how it looks.

use super::*;
use crate::server::{Call, CallState, Peer, Stats};
use egui_kittest::{Harness, kittest::Queryable};
use rusttalk_core::diagram::{Access, Doc, Edge, Feed, Item, Kind, Op, Shape};
use tokio::sync::mpsc::UnboundedReceiver;

type Screen = (Home, crate::update::Updater, crate::keystore::Keystore);

const ANN_DEVICE: &str = "ann-device";

fn in_call(
    dark: bool,
    size: Vec2,
    data: &std::path::Path,
    ann_hosts: bool,
) -> (Harness<'static, Option<Screen>>, UnboundedReceiver<Command>) {
    let (mut base, sent) = files_harness(dark);
    let mut home = base.state_mut().take().unwrap();
    home.view = View::Server(0);
    home.layout.list = false;
    home.servers[0].open = Some(PROJECTS);
    home.servers[0].dir.categories[0].channels[0].voice = true;
    home.find.opened = false;
    home.voice.call = Some(Call {
        server: home.servers[0].address.clone(),
        channel: PROJECTS,
        state: CallState::Connected,
        stats: Stats::default(),
        can_talk: true,
        can_share_video: true,
    });
    let peer = |person, diagram: Option<&str>| Peer {
        person,
        connecting: false,
        unknown: false,
        status: Default::default(),
        apps: vec![],
        camera: None,
        terminal: None,
        diagram: diagram.map(str::to_owned),
        screens: vec![],
    };
    let ann = peer(ANN, ann_hosts.then_some(ANN_DEVICE));
    home.servers[0]
        .voice
        .insert(PROJECTS, vec![peer(ME, None), ann]);
    let mut prefs = home.notify.prefs().clone();
    prefs.desktop = false;
    home.notify.set_prefs(prefs);
    let mut settings = home.voice.audio.settings();
    settings.set_appearance(ui::Appearance {
        mode: if dark {
            ui::Mode::Dark
        } else {
            ui::Mode::Light
        },
        ..Default::default()
    });
    home.voice.audio.set_settings(settings);
    let mut h = Harness::builder().with_size(size).build_ui_state(
        |ui, screen: &mut Option<Screen>| {
            if let Some((home, updates, keys)) = screen {
                show(ui, home, updates, keys);
            }
        },
        None,
    );
    ui::install(&h.ctx);
    *h.state_mut() = Some((
        home,
        crate::update::Updater::new(data),
        crate::keystore::without_keychain(data),
    ));
    h.run_steps(4);
    (h, sent)
}

fn home_mut<'a>(h: &'a mut Harness<'_, Option<Screen>>) -> &'a mut Home {
    &mut h.state_mut().as_mut().unwrap().0
}

fn update(h: &mut Harness<'_, Option<Screen>>, update: Update) {
    let ctx = h.ctx.clone();
    let now = ctx.input(|input| input.time);
    home_mut(h).apply_update(&ctx, update, now);
    h.run_steps(4);
}

fn sent(sent: &mut UnboundedReceiver<Command>) -> Vec<Command> {
    std::iter::from_fn(|| sent.try_recv().ok()).collect()
}

fn capture(h: &mut Harness<'_, Option<Screen>>, size: Vec2, dark: bool, state: &str) {
    if let Ok(dir) = std::env::var("RUSTTALK_DIAGRAM_SCREENSHOTS") {
        std::fs::create_dir_all(&dir).unwrap();
        let theme = if dark { "dark" } else { "light" };
        let name = format!("{dir}/{state}-{}x{}-{theme}.png", size.x, size.y);
        h.render().unwrap().save(name).unwrap();
    }
}

/// A small flow chart.
fn sample() -> Doc {
    let mut doc = Doc::default();
    let shapes = [
        (1, Kind::Rounded, 0.0, 0.0, 1, "Idea"),
        (2, Kind::Diamond, 200.0, -10.0, 3, "Worth it?"),
        (3, Kind::Rect, 400.0, -60.0, 2, "Build it"),
        (4, Kind::Ellipse, 400.0, 60.0, 5, "Drop it"),
        (5, Kind::Text, 0.0, 110.0, 0, "Drawn together\nin the call"),
    ];
    for (id, kind, x, y, color, text) in shapes {
        doc.apply(&Op::Put {
            item: Item::Shape(Shape {
                id,
                kind,
                x,
                y,
                w: 140.0,
                h: if kind == Kind::Diamond { 90.0 } else { 70.0 },
                text: text.into(),
                color,
            }),
        });
    }
    for (id, from, to) in [(10, 1, 2), (11, 2, 3), (12, 2, 4)] {
        doc.apply(&Op::Put {
            item: Item::Edge(Edge {
                id,
                from,
                to,
                color: 0,
            }),
        });
    }
    doc
}

#[test]
fn starting_a_diagram_presents_it_and_edits_go_out() {
    let data = tempfile::tempdir().unwrap();
    for (size, dark) in [Vec2::new(1100.0, 700.0), Vec2::new(390.0, 844.0)]
        .into_iter()
        .flat_map(|size| [(size, true), (size, false)])
    {
        let (mut h, mut commands) = in_call(dark, size, data.path(), false);
        sent(&mut commands);
        // Available without a terminal, from the "…" menu.
        h.get_by_label(t!("call.more")).click();
        h.run_steps(2);
        h.get_by_label(t!("diagram.start")).click();
        h.run_steps(2);
        assert!(
            sent(&mut commands)
                .iter()
                .any(|c| matches!(c, Command::StartDiagram(doc) if doc.is_empty()))
        );
        assert!(home_mut(&mut h).voice.diagrams.hosting);
        // The call says it started, and the roster shows it.
        home_mut(&mut h).servers[0]
            .voice
            .get_mut(&PROJECTS)
            .unwrap()[0]
            .diagram = Some("me-device".into());
        update(&mut h, Update::Diagram(Feed::Reset(sample())));
        assert!(home_mut(&mut h).voice.stage.is_presenting());
        h.run_steps(4);
        for tool in [
            "diagram.rectangle",
            "diagram.ellipse",
            "diagram.diamond",
            "diagram.connect",
            "diagram.undo",
            "diagram.end",
        ] {
            assert!(
                h.query_by_label(crate::i18n::text(tool)).is_some(),
                "{tool}"
            );
        }
        capture(&mut h, size, dark, "host");

        // Deleting a shape sends the edit, with its connectors gone too.
        home_mut(&mut h).voice.diagrams.canvas.selected = Some(2);
        h.run_steps(1);
        h.get_by_label(t!("diagram.delete")).click();
        h.run_steps(2);
        let edits: Vec<_> = sent(&mut commands)
            .into_iter()
            .filter_map(|c| match c {
                Command::EditDiagram(ops) => Some(ops),
                _ => None,
            })
            .collect();
        assert_eq!(edits, [vec![Op::Delete { id: 2 }]]);
        let doc = home_mut(&mut h).voice.diagrams.doc.clone().unwrap();
        assert_eq!((doc.shapes.len(), doc.edges.len()), (4, 0));
        // Undo puts it back, connectors included.
        h.get_by_label(t!("diagram.undo")).click();
        h.run_steps(2);
        let doc = home_mut(&mut h).voice.diagrams.doc.clone().unwrap();
        assert_eq!((doc.shapes.len(), doc.edges.len()), (5, 3));

        // Someone asks to edit: the host is asked, and a no goes back.
        let ask = crate::net::diagram::Guests {
            asking: vec![(ANN_DEVICE.into(), ANN)],
            editing: vec![],
        };
        update(&mut h, Update::DiagramGuests(ask));
        assert!(h.query_by_label(t!("remote.allow")).is_some());
        if size.x > 1000.0 {
            capture(&mut h, size, dark, "host-request");
        }
        h.get_by_label(t!("remote.deny")).click();
        h.run_steps(2);
        assert!(
            sent(&mut commands)
                .iter()
                .any(|c| matches!(c, Command::AnswerEdit(who, false) if who == ANN_DEVICE))
        );

        // Ending it leaves presentation mode.
        h.get_by_label(t!("diagram.end")).click();
        h.run_steps(2);
        assert!(
            sent(&mut commands)
                .iter()
                .any(|c| matches!(c, Command::StopDiagram))
        );
        assert!(!home_mut(&mut h).voice.diagrams.open());
        assert!(!home_mut(&mut h).voice.stage.is_presenting());
    }
}

#[test]
fn others_join_on_their_own_ask_to_edit_and_see_the_end() {
    let data = tempfile::tempdir().unwrap();
    for (size, dark) in [Vec2::new(1100.0, 700.0), Vec2::new(390.0, 844.0)]
        .into_iter()
        .flat_map(|size| [(size, true), (size, false)])
    {
        let (mut h, mut commands) = in_call(dark, size, data.path(), true);
        h.run_steps(2);
        // Someone in the call hosts one: it is joined without asking.
        assert!(
            sent(&mut commands)
                .iter()
                .any(|c| matches!(c, Command::JoinDiagram(Some(host)) if host == ANN_DEVICE))
        );
        // The late joiner gets the current diagram, shown big for everyone.
        update(&mut h, Update::Diagram(Feed::Reset(sample())));
        assert!(home_mut(&mut h).voice.stage.is_presenting());
        assert!(h.query_by_label(t!("diagram.rectangle")).is_none());
        capture(&mut h, size, dark, "viewer");

        h.get_by_label(t!("diagram.ask_to_edit")).click();
        h.run_steps(2);
        assert!(
            sent(&mut commands)
                .iter()
                .any(|c| matches!(c, Command::AskToEdit))
        );
        update(&mut h, Update::Diagram(Feed::Editing(Access::Granted)));
        assert!(h.query_by_label(t!("diagram.rectangle")).is_some());
        assert!(h.query_by_label(t!("diagram.stop_editing")).is_some());
        capture(&mut h, size, dark, "editor");

        // An edit from the host arrives.
        let moved = Op::Delete { id: 4 };
        update(&mut h, Update::Diagram(Feed::Ops(vec![moved])));
        let doc = home_mut(&mut h).voice.diagrams.doc.clone().unwrap();
        assert!(doc.shape(4).is_none());

        // The host ends it: a note, and back to the call.
        update(&mut h, Update::Diagram(Feed::Ended));
        let home = home_mut(&mut h);
        assert!(!home.voice.diagrams.open());
        assert!(
            home.toast
                .as_ref()
                .is_some_and(|(note, _)| note.contains("Ann") || note == t!("diagram.ended"))
        );
        home.servers[0].voice.get_mut(&PROJECTS).unwrap()[1].diagram = None;
        h.run_steps(4);
        assert!(!home_mut(&mut h).voice.stage.is_presenting());
    }
}

#[test]
fn leaving_returns_to_the_call_and_is_not_undone() {
    let data = tempfile::tempdir().unwrap();
    let size = Vec2::new(1100.0, 700.0);
    let (mut h, mut commands) = in_call(true, size, data.path(), true);
    h.run_steps(2);
    update(&mut h, Update::Diagram(Feed::Reset(sample())));
    sent(&mut commands);
    h.get_by_label(t!("diagram.leave")).click();
    h.run_steps(4);
    assert!(
        sent(&mut commands)
            .iter()
            .any(|c| matches!(c, Command::JoinDiagram(None)))
    );
    // Still in the call, not presenting, and not joined again on its own.
    let home = home_mut(&mut h);
    assert!(home.voice.call.is_some());
    assert!(!home.voice.diagrams.open());
    assert!(!home.voice.stage.is_presenting());
    h.run_steps(4);
    assert!(
        !sent(&mut commands)
            .iter()
            .any(|c| matches!(c, Command::JoinDiagram(_)))
    );
    capture(&mut h, size, true, "left");
}
