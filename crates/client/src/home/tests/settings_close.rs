//! Closing Settings and Server settings: Escape in a text field only leaves it, and
//! unsaved changes ask "Discard changes?" first.

use super::*;
use egui_kittest::{
    Harness,
    kittest::{Queryable, by},
};

/// The whole screen at `size` with `set` applied to the Friends server's home.
fn screen(size: Vec2, dark: bool, set: impl FnOnce(&mut Home)) -> Harness<'static, Option<Screen>> {
    let (mut files, _sent) = files_harness(dark);
    let mut home = files.state_mut().take().unwrap();
    home.view = View::Server(0);
    home.servers[0].open = Some(PROJECTS);
    // Never a search on the real directory from a test.
    home.find.opened = false;
    let mut prefs = home.notify.prefs().clone();
    prefs.desktop = false;
    home.notify.set_prefs(prefs);
    set(&mut home);
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
    harness
}

fn home<'h>(h: &'h Harness<'_, Option<Screen>>) -> &'h Home {
    &h.state().as_ref().unwrap().0
}

/// Server settings on its public listing, as the owner, with a saved listing.
fn listing(home: &mut Home) {
    home.servers[0].dir.perms.owner = true;
    crate::admin::open(home, 0, crate::admin::Tab::Listing);
    let listing = crate::net::PublicListing {
        name: "Friends".into(),
        description: "A place for friends.".into(),
        address: "https://friends.example.org/".into(),
        index_url: crate::official::DIRECTORY.into(),
        index_fingerprint: String::new(),
    };
    (home.server_settings.as_mut().unwrap()).apply(crate::net::AdminUpdate::Listing(
        true,
        Some(listing),
        None,
    ));
}

/// Server settings on Roles, with a custom role below this person's.
fn roles(home: &mut Home) {
    let perms = &mut home.servers[0].dir.perms;
    perms.manage_roles = true;
    perms.rank = 100;
    home.servers[0].dir.roles = vec![crate::server::Role {
        id: "fans".into(),
        name: "Fans".into(),
        name_known: true,
        rank: 10,
        permissions: Vec::new(),
        custom: true,
        color: None,
        separate: false,
    }];
    crate::admin::open(home, 0, crate::admin::Tab::Roles);
}

/// A click on the dimmed window around the settings window.
fn click_outside(h: &mut Harness<'_, Option<Screen>>) {
    let corner = egui::pos2(6.0, 6.0);
    h.hover_at(corner);
    h.drag_at(corner);
    h.drop_at(corner);
    h.run_steps(4);
}

fn type_into(h: &mut Harness<'_, Option<Screen>>, label: &str, text: &str) {
    h.get_by_label(label).click();
    h.run_steps(3);
    h.get_by_label(label).type_text(text);
    h.run_steps(3);
}

#[test]
fn escape_in_a_text_field_keeps_server_settings_and_the_text() {
    let mut h = screen(Vec2::new(1100.0, 700.0), true, listing);
    type_into(
        &mut h,
        t!("admin.public_description"),
        " Board games on Fridays.",
    );
    h.key_press(Key::Escape);
    h.run_steps(4);
    let settings = home(&h).server_settings.as_ref();
    assert!(settings.is_some(), "Escape in a field closed the window");
    assert!(h.query_by_label(t!("settings.discard_title")).is_none());
    assert_eq!(
        h.get_by_label(t!("admin.public_description"))
            .value()
            .as_deref(),
        Some("A place for friends. Board games on Fridays.")
    );

    // The field has let go: Escape now closes, after asking about the description.
    h.key_press(Key::Escape);
    h.run_steps(4);
    h.get_by_label(t!("settings.discard_title"));
    assert!(home(&h).server_settings.is_some());
    h.get_by_label(t!("settings.keep_editing")).click();
    h.run_steps(4);
    assert!(h.query_by_label(t!("settings.discard_title")).is_none());
    assert!(home(&h).server_settings.is_some());
    assert_eq!(
        h.get_by_label(t!("admin.public_description"))
            .value()
            .as_deref(),
        Some("A place for friends. Board games on Fridays.")
    );
}

#[test]
fn a_click_outside_with_unsaved_role_edits_asks_first() {
    for (size, dark) in [(390.0, 844.0), (1100.0, 700.0)]
        .into_iter()
        .flat_map(|(w, h)| [(Vec2::new(w, h), true), (Vec2::new(w, h), false)])
    {
        let mut h = screen(size, dark, roles);
        type_into(&mut h, t!("admin.role_name"), " club");
        let phone = size.x < ui::PHONE_WIDTH;
        if phone {
            // Phones have no outside to click: the × closes the sheet.
            h.get_by_label(t!("common.close")).click();
            h.run_steps(4);
        } else {
            click_outside(&mut h);
        }
        h.get_by_label(t!("settings.discard_title"));
        assert!(home(&h).server_settings.is_some());
        if let Some(dir) = std::env::var_os("RUSTTALK_DISCARD_SCREENSHOTS") {
            let theme = if dark { "dark" } else { "light" };
            h.render()
                .unwrap()
                .save(
                    std::path::PathBuf::from(dir)
                        .join(format!("discard-{theme}-{}x{}.png", size.x, size.y)),
                )
                .unwrap();
        }

        // Keep editing goes back to the role, still changed.
        h.get_by_label(t!("settings.keep_editing")).click();
        h.run_steps(4);
        assert!(h.query_by_label(t!("settings.discard_title")).is_none());
        assert_eq!(
            h.get_by_label(t!("admin.role_name")).value().as_deref(),
            Some("Fans club")
        );

        // Discard closes and drops the change.
        if phone {
            h.get_by_label(t!("common.close")).click();
            h.run_steps(4);
        } else {
            click_outside(&mut h);
        }
        h.get_by_label(t!("settings.discard")).click();
        h.run_steps(4);
        assert!(home(&h).server_settings.is_none());
        assert!(h.query_by_label(t!("settings.discard_title")).is_none());
        let screen = h.state_mut().as_mut().unwrap();
        crate::admin::open(&mut screen.0, 0, crate::admin::Tab::Roles);
        h.run_steps(4);
        h.get_by_label(t!("admin.role_name")).click();
        h.run_steps(3);
        assert_eq!(
            h.get_by_label(t!("admin.role_name")).value().as_deref(),
            Some("Fans")
        );
    }
}

#[test]
fn nothing_unsaved_closes_at_once() {
    let mut h = screen(Vec2::new(1100.0, 700.0), false, listing);
    h.key_press(Key::Escape);
    h.run_steps(4);
    assert!(home(&h).server_settings.is_none());
    assert!(h.query_by_label(t!("settings.discard_title")).is_none());

    let mut h = screen(Vec2::new(1100.0, 700.0), false, |home| {
        home.settings = Some(Tab::Profile);
    });
    h.key_press(Key::Escape);
    h.run_steps(4);
    assert!(home(&h).settings.is_none());
}

#[test]
fn an_unsaved_bio_asks_before_settings_close() {
    let mut h = screen(Vec2::new(1100.0, 700.0), true, |home| {
        home.settings = Some(Tab::Profile);
    });
    // The row's title has the same name as the field.
    let bio = || {
        by().role(Role::MultilineTextInput)
            .label(t!("settings.bio"))
    };
    h.get(bio()).click();
    h.run_steps(3);
    h.get(bio()).type_text("Hi there");
    h.run_steps(3);
    h.key_press(Key::Escape);
    h.run_steps(4);
    assert!(
        home(&h).settings.is_some(),
        "Escape in the bio closed Settings"
    );
    h.key_press(Key::Escape);
    h.run_steps(4);
    h.get_by_label(t!("settings.discard_title"));
    h.get_by_label(t!("settings.discard")).click();
    h.run_steps(4);
    assert!(home(&h).settings.is_none());
    assert!(home(&h).bio_draft.is_none());
}
