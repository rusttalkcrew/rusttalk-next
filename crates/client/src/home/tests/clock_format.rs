//! Settings → Appearance → Time format: Automatic, 12-hour or 24-hour (#668).

use super::*;
use chrono::TimeZone;
use egui_kittest::{
    Harness,
    kittest::{Queryable, by},
};

/// Today at `hour`:`minute`, so the chat shows only the clock.
fn today(hour: u32, minute: u32) -> i64 {
    let at = Local::now()
        .date_naive()
        .and_hms_opt(hour, minute, 0)
        .unwrap();
    Local
        .from_local_datetime(&at)
        .earliest()
        .unwrap()
        .timestamp()
}

fn text(id: u8, person: Id, at: i64, text: &str) -> Msg {
    Msg {
        backfilled: false,
        id: [id; 32],
        person,
        at,
        reply_to: None,
        quoted_author: None,
        edit_of: None,
        body: Body::Text(text.into()),
        files: Vec::new(),
        live: None,
        sticker: None,
        replicas: Vec::new(),
        copy_files: Vec::new(),
    }
}

/// The whole app at `size` on the Friends server's chat, with `clock` picked.
fn screen(size: Vec2, dark: bool, clock: &str) -> Harness<'static, Option<Screen>> {
    let (mut files, _sent) = files_harness(dark);
    let mut home = files.state_mut().take().unwrap();
    home.view = View::Server(0);
    home.servers[0].open = Some(PROJECTS);
    home.find.opened = false;
    let mut prefs = home.notify.prefs().clone();
    prefs.desktop = false;
    home.notify.set_prefs(prefs);
    let mut settings = home.voice.audio.settings();
    settings.clock = clock.into();
    home.voice.audio.set_settings(settings);
    let chat = home.servers[0].chats.get_mut(&PROJECTS).unwrap();
    chat.at_start = true;
    chat.merge(
        vec![
            text(1, ANN, today(9, 5), "Morning! Quiz tonight?"),
            text(2, ME, today(14, 58), "Yes, see you at eight."),
        ],
        false,
    );
    let data = tempfile::tempdir().unwrap();
    let mut harness = Harness::builder()
        .with_size(size)
        .with_context_setup(ui::install)
        .with_theme(if dark {
            egui::Theme::Dark
        } else {
            egui::Theme::Light
        })
        .build_ui_state(
            |ui, screen: &mut Option<Screen>| {
                if let Some((home, updates, keystore)) = screen {
                    show(ui, home, updates, keystore);
                }
            },
            None,
        );
    *harness.state_mut() = Some((
        home,
        crate::update::Updater::new(data.path()),
        crate::keystore::without_keychain(data.path()),
    ));
    harness.run_steps(6);
    // Phones open on the channel list; show the chat as after picking the channel.
    home_mut(&mut harness).layout.list = false;
    harness.run_steps(4);
    harness
}

fn home_mut<'h>(h: &'h mut Harness<'_, Option<Screen>>) -> &'h mut Home {
    &mut h.state_mut().as_mut().unwrap().0
}

fn open_appearance(h: &mut Harness<'_, Option<Screen>>) {
    home_mut(h).settings = Some(Tab::Appearance);
    h.run_steps(4);
}

#[test]
fn the_picker_saves_the_clock_format() {
    let mut h = screen(Vec2::new(1100.0, 700.0), true, "auto");
    open_appearance(&mut h);
    let combo = || {
        by().role(egui::accesskit::Role::ComboBox)
            .label(t!("settings.clock"))
    };
    // English follows its 12-hour clock until another choice is made.
    assert_eq!(
        h.get(combo()).value().as_deref(),
        Some("Automatic (2:58 PM)")
    );
    for (label, id) in [
        ("24-hour (14:58)", "24h"),
        ("12-hour (2:58 PM)", "12h"),
        ("Automatic (2:58 PM)", "auto"),
    ] {
        h.get(combo()).click();
        h.run_steps(2);
        h.query_all_by_label(label).last().unwrap().click();
        h.run_steps(3);
        assert_eq!(home_mut(&mut h).voice.audio.settings().clock, id, "{label}");
        assert_eq!(h.get(combo()).value().as_deref(), Some(label));
    }
}

/// `RUSTTALK_CLOCK_SCREENSHOTS=<dir>`: the setting, and the chat in each format.
/// The clock format is per thread in tests, so this runs beside the others.
#[test]
fn clock_format_screenshots() {
    let Ok(dir) = std::env::var("RUSTTALK_CLOCK_SCREENSHOTS") else {
        return;
    };
    std::fs::create_dir_all(&dir).unwrap();
    for (theme, dark) in [("dark", true), ("light", false)] {
        for (tag, size) in [
            ("desktop-1100x700", Vec2::new(1100.0, 700.0)),
            ("phone-390x844", Vec2::new(390.0, 844.0)),
        ] {
            for clock in ["12h", "24h"] {
                let mut h = screen(size, dark, clock);
                h.run_steps(2);
                let path = format!("{dir}/chat-{clock}-{tag}-{theme}.png");
                h.render().unwrap().save(path).unwrap();
            }
            let mut h = screen(size, dark, "24h");
            open_appearance(&mut h);
            h.get(
                by().role(egui::accesskit::Role::ComboBox)
                    .label(t!("settings.clock")),
            )
            .click();
            h.run_steps(3);
            let path = format!("{dir}/appearance-{tag}-{theme}.png");
            h.render().unwrap().save(path).unwrap();
        }
    }
}
