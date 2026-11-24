//! Phones get the call's controls in the call view, with the chat hidden at first
//! (#513); tablets and desktops keep them in the sidebar's voice panel.

use super::*;
use crate::server::{Call, CallState, Peer, Stats};
use egui_kittest::{Harness, kittest::Queryable};

type Screen = (Home, crate::update::Updater, crate::keystore::Keystore);

fn in_call(dark: bool, size: Vec2, data: &std::path::Path) -> Harness<'static, Option<Screen>> {
    let (mut base, _sent) = files_harness(dark);
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
    let peer = |person| Peer {
        person,
        connecting: false,
        unknown: false,
        status: Default::default(),
        apps: vec![],
        camera: None,
        terminal: None,
        screens: vec![],
    };
    home.servers[0]
        .voice
        .insert(PROJECTS, vec![peer(ME), peer(ANN)]);
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
    h
}

fn home<'a>(h: &'a Harness<'_, Option<Screen>>) -> &'a Home {
    &h.state().as_ref().unwrap().0
}

fn capture(h: &mut Harness<'_, Option<Screen>>, size: Vec2, dark: bool, state: &str) {
    if let Ok(dir) = std::env::var("RUSTTALK_CALL_CONTROLS_SCREENSHOTS") {
        std::fs::create_dir_all(&dir).unwrap();
        let theme = if dark { "dark" } else { "light" };
        let name = format!("{dir}/{}x{}-{theme}-{state}.png", size.x, size.y);
        h.render().unwrap().save(name).unwrap();
    }
}

#[test]
fn phones_show_call_controls_and_hide_the_chat_at_first() {
    let data = tempfile::tempdir().unwrap();
    for dark in [true, false] {
        let size = Vec2::new(390.0, 844.0);
        let mut h = in_call(dark, size, data.path());
        assert!(home(&h).layout.phone);
        // One mic and one leave button: the call view's, the sidebar is hidden.
        let mute = h.get_by_label(t!("call.mute")).rect();
        let leave = h.get_by_label(t!("call.leave")).rect();
        assert!(mute.height() >= ui::TOUCH_TARGET && leave.height() >= ui::TOUCH_TARGET);
        assert!(leave.bottom() > size.y - 40.0 && leave.right() <= size.x);
        assert!((mute.center().y - leave.center().y).abs() < 1.0, "one row");
        assert!(h.query_by_label(t!("call.deafen")).is_some());
        // The chat starts hidden; the desktop choice is left alone.
        assert!(h.query_by_label(t!("layout.show_chat")).is_some());
        assert!(home(&h).layout.call_split().chat_hidden);
        assert!(!home(&h).layout.split.chat_hidden);
        capture(&mut h, size, dark, "call");

        h.get_by_label(t!("call.mute")).click();
        h.run_steps(2);
        assert!(home(&h).voice.audio.muted());
        assert!(h.query_by_label(t!("call.unmute")).is_some());

        h.get_by_label(t!("layout.show_chat")).click();
        h.run_steps(3);
        assert!(!home(&h).layout.call_split().chat_hidden);
        assert!(h.query_by_label(t!("layout.hide_chat")).is_some());
        // The controls stay below the call and its chat.
        let leave = h.get_by_label(t!("call.leave")).rect();
        assert!(leave.bottom() > size.y - 40.0);
        capture(&mut h, size, dark, "chat");

        h.get_by_label(t!("call.leave")).click();
        h.run_steps(2);
        assert!(home(&h).voice.call.is_none());
    }
}

#[test]
fn tablets_and_desktops_keep_controls_in_the_sidebar() {
    let data = tempfile::tempdir().unwrap();
    for dark in [true, false] {
        for size in [Vec2::new(820.0, 1180.0), Vec2::new(1100.0, 700.0)] {
            let mut h = in_call(dark, size, data.path());
            assert!(!home(&h).layout.phone);
            // The chat shows beside the call as before, and the only leave button is
            // the voice panel's in the sidebar.
            assert!(!home(&h).layout.call_split().chat_hidden);
            assert!(h.query_by_label(t!("layout.hide_chat")).is_some());
            let leave = h.get_by_label(t!("call.leave")).rect();
            let sidebar = RAIL_WIDTH + home(&h).layout.channels;
            assert!(leave.right() <= sidebar, "{size:?}: {leave:?}");
            assert!(leave.height() < ui::TOUCH_TARGET);
            capture(&mut h, size, dark, "call");
        }
    }
}
