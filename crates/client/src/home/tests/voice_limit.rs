//! A voice channel's member limit and bitrate: clearing the limit removes it, the channel list
//! says that "2/8" is a limit, and music channels keep up to 320 kbps. Set
//! RUSTTALK_TEST_SCREENSHOTS to save how they look.

use super::*;
use egui_kittest::{
    Harness,
    kittest::{NodeT, Queryable, by},
};

const LOUNGE: Id = [9; 32];

type Sent = tokio::sync::mpsc::UnboundedReceiver<Command>;

/// A manager's server with a voice channel limited to 8 that Me and Ann are in.
fn limited_home(dark: bool) -> (Home, Sent) {
    let (mut fixture, sent) = files_harness(dark);
    let mut home = fixture.state_mut().take().unwrap();
    home.view = View::Server(0);
    let server = &mut home.servers[0];
    server.dir.perms.manage_channels = true;
    server.dir.categories[0]
        .channels
        .push(crate::server::Channel {
            id: LOUNGE,
            name: "Lounge".into(),
            topic: String::new(),
            voice: true,
            page: false,
            forum: false,
            audio_kbps: 64,
            member_limit: 8,
            slow_mode_secs: 0,
            focus_until: None,
            flags: Vec::new(),
        });
    let peer = |person| crate::server::Peer {
        person,
        connecting: false,
        unknown: false,
        status: Default::default(),
        apps: Vec::new(),
        screens: Vec::new(),
        camera: None,
        terminal: None,
        diagram: None,
    };
    server.voice.insert(LOUNGE, vec![peer(ME), peer(ANN)]);
    (home, sent)
}

fn theme(dark: bool) -> egui::Theme {
    if dark {
        egui::Theme::Dark
    } else {
        egui::Theme::Light
    }
}

fn screenshot(h: &mut Harness<'_, Home>, name: &str, size: Vec2, dark: bool) {
    if let Ok(dir) = std::env::var("RUSTTALK_TEST_SCREENSHOTS") {
        std::fs::create_dir_all(&dir).unwrap();
        let theme = if dark { "dark" } else { "light" };
        let path = format!("{dir}/{name}-{theme}-{}x{}.png", size.x, size.y);
        h.render().unwrap().save(path).unwrap();
    }
}

/// The server rail and channel list, as the window shows them.
fn channel_list_harness(size: Vec2, dark: bool) -> Harness<'static, Home> {
    let (home, _sent) = limited_home(dark);
    let mut h = Harness::builder()
        .with_size(size)
        .with_context_setup(ui::install)
        .with_theme(theme(dark))
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

#[test]
fn a_limited_voice_channel_says_its_count_is_a_limit() {
    let tip = t!("call.member_limit_tip", limit = 8);
    for size in [Vec2::new(390.0, 844.0), Vec2::new(1100.0, 700.0)] {
        for dark in [true, false] {
            let mut h = channel_list_harness(size, dark);
            assert!(h.query_by_label("👥").is_some());
            assert!(h.query_by_label(&tip).is_none());
            h.get_by_label("2/8").hover();
            h.run_steps(3);
            assert!(h.query_by_label(&tip).is_some());
            screenshot(&mut h, "voice-limit-row", size, dark);
        }
    }
}

#[test]
fn on_a_touch_screen_a_tap_shows_the_limit_until_the_next_tap() {
    let tip = t!("call.member_limit_tip", limit = 8);
    let mut h = channel_list_harness(Vec2::new(390.0, 844.0), true);
    // Any touch tells egui this is a touch screen.
    for phase in [egui::TouchPhase::Start, egui::TouchPhase::End] {
        h.event(egui::Event::Touch {
            device_id: egui::TouchDeviceId(0),
            id: egui::TouchId(0),
            phase,
            pos: egui::pos2(380.0, 830.0),
            force: None,
        });
    }
    h.run_steps(2);
    h.get_by_label("2/8").click();
    h.run_steps(2);
    // The finger is gone, so nothing hovers it any more; the tip stays.
    h.event(egui::Event::PointerGone);
    // A first pass sizes the tip, the next shows it.
    h.run_steps(3);
    screenshot(&mut h, "voice-limit-tapped", Vec2::new(390.0, 844.0), true);
    // On touch the count is a button named by the tip; the open tip is a second node.
    let named = |h: &Harness<'_, Home>| h.query_all_by_label(&tip).count();
    assert_eq!(named(&h), 2);
    h.get_by_label("#  projects").click();
    h.run_steps(2);
    h.event(egui::Event::PointerGone);
    h.run_steps(2);
    assert_eq!(named(&h), 1);
}

/// The Edit channel dialog of the limited voice channel.
fn dialog_harness(size: Vec2, dark: bool) -> (Harness<'static, Home>, Sent) {
    let (mut home, sent) = limited_home(dark);
    home.edit_channel(0, LOUNGE, ChannelEdit::Rename);
    let mut h = Harness::builder()
        .with_size(size)
        .with_context_setup(ui::install)
        .with_theme(theme(dark))
        // The channel name field has no accessible name yet (not this test's concern).
        .with_accessibility_check(false)
        .build_ui_state(|ui, home| dialogs(ui.ctx(), home), home);
    h.run_steps(3);
    (h, sent)
}

fn spin<'a>(h: &'a Harness<'static, Home>) -> egui_kittest::Node<'a> {
    h.get(by().role(Role::SpinButton).label(t!("home.member_limit")))
}

/// Save, and the member limit the dialog sent: None when it sent no change.
fn saved_limit(h: &mut Harness<'static, Home>, sent: &mut Sent) -> Option<u32> {
    h.get_by_label(t!("common.save")).click();
    h.run_steps(3);
    std::iter::from_fn(|| sent.try_recv().ok()).find_map(|command| match command {
        Command::RenameChannel { member_limit, .. } => member_limit,
        _ => None,
    })
}

#[test]
fn clearing_the_member_limit_removes_it() {
    for size in [Vec2::new(390.0, 844.0), Vec2::new(1100.0, 700.0)] {
        for dark in [true, false] {
            let (mut h, mut sent) = dialog_harness(size, dark);
            spin(&h).click();
            h.run_steps(2);
            h.key_press(egui::Key::Backspace);
            h.key_press(egui::Key::Enter);
            h.run_steps(3);
            assert_eq!(spin(&h).accesskit_node().numeric_value(), Some(0.0));
            screenshot(&mut h, "voice-limit-cleared", size, dark);
            assert_eq!(saved_limit(&mut h, &mut sent), Some(0));
        }
    }
}

#[test]
fn typed_limits_and_no_limit_parse() {
    assert_eq!(no_limit_parser(""), Some(0.0));
    assert_eq!(no_limit_parser("  "), Some(0.0));
    assert_eq!(no_limit_parser(t!("home.no_limit")), Some(0.0));
    assert_eq!(no_limit_parser(" 12 "), Some(12.0));
    // Anything else keeps the old value, as before.
    assert_eq!(no_limit_parser("lots"), None);
    assert_eq!(no_limit_parser("-3"), None);
}

fn slider<'a>(h: &'a Harness<'static, Home>) -> egui_kittest::Node<'a> {
    h.get(
        by().role(Role::Slider)
            .label(t!("home.audio_quality_label")),
    )
}

/// The Edit channel dialog of the voice channel set to `kbps`.
fn bitrate_harness(kbps: u32, size: Vec2, dark: bool) -> (Harness<'static, Home>, Sent) {
    let (mut home, sent) = limited_home(dark);
    let lounge = home.servers[0].dir.categories[0]
        .channels
        .last_mut()
        .unwrap();
    lounge.audio_kbps = kbps;
    home.edit_channel(0, LOUNGE, ChannelEdit::Rename);
    let mut h = Harness::builder()
        .with_size(size)
        .with_context_setup(ui::install)
        .with_theme(theme(dark))
        .with_accessibility_check(false)
        .build_ui_state(|ui, home| dialogs(ui.ctx(), home), home);
    h.run_steps(3);
    (h, sent)
}

/// Save, and the bitrate the dialog sent: None when it sent no change.
fn saved_kbps(h: &mut Harness<'static, Home>, sent: &mut Sent) -> Option<u32> {
    h.get_by_label(t!("common.save")).click();
    h.run_steps(3);
    std::iter::from_fn(|| sent.try_recv().ok()).find_map(|command| match command {
        Command::RenameChannel { audio_kbps, .. } => audio_kbps,
        _ => None,
    })
}

#[test]
fn a_music_channel_keeps_its_bitrate_when_other_fields_are_saved() {
    for size in [Vec2::new(390.0, 844.0), Vec2::new(1100.0, 700.0)] {
        for dark in [true, false] {
            let (mut h, mut sent) = bitrate_harness(320, size, dark);
            assert_eq!(slider(&h).accesskit_node().numeric_value(), Some(320.0));
            assert!(
                h.query_by_label(&t!("home.audio_quality", kbps = 320))
                    .is_some()
            );
            assert!(h.query_by_label(t!("home.audio_quality_help")).is_some());
            screenshot(&mut h, "audio-quality-320", size, dark);
            // Change only the member limit.
            spin(&h).click();
            h.run_steps(2);
            h.key_press(egui::Key::Backspace);
            h.key_press(egui::Key::Enter);
            h.run_steps(3);
            assert_eq!(saved_kbps(&mut h, &mut sent), None);
        }
    }
}

#[test]
fn the_bitrate_slider_goes_past_128() {
    let (mut h, mut sent) = bitrate_harness(128, Vec2::new(1100.0, 700.0), true);
    slider(&h).focus();
    h.run_steps(2);
    h.key_press(egui::Key::ArrowRight);
    h.run_steps(2);
    assert_eq!(slider(&h).accesskit_node().numeric_value(), Some(129.0));
    assert_eq!(saved_kbps(&mut h, &mut sent), Some(129));
}
