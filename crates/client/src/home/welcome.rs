//! "What should people call you?" (#201): the default name, asked once right after a
//! new account saved its recovery words, then an optional bio. A restored device or
//! an older account is only asked when the synced account data has no default name
//! and none could be taken from the names it already uses on its servers.

use eframe::egui::Ui;

use super::{Home, Join, View};
use crate::{
    app::submitted,
    net::Command,
    profile::{BIO_CHARS, NAME_CHARS, clean_bio, fit_bio},
    ui::{self, error_text, primary_large, secondary_large},
};

/// Where the person is in the steps.
#[derive(Default)]
pub enum Step {
    #[default]
    Idle,
    Name(String),
    Bio(String),
}

/// Whether to start asking now: never with a default name. A new account asks at
/// once; other devices wait for the synced account data and, so names already used
/// can become the default, for every server to load or the join page to be open.
pub fn due(name_set: bool, new_account: bool, synced: bool, loaded: bool, joining: bool) -> bool {
    !name_set && (new_account || synced && (loaded || joining))
}

impl Home {
    fn welcome_due(&mut self) -> bool {
        let settings = self.voice.audio.settings();
        let loaded = !self.servers.is_empty()
            && self
                .servers
                .iter()
                .all(crate::server::Server::profile_ready);
        let joining = self.view == View::Join && matches!(self.join, Join::Idle(_));
        let synced = self.synced_once;
        if !due(
            !settings.display_name.is_empty(),
            self.new_account,
            synced,
            loaded,
            joining,
        ) {
            return false;
        }
        // Names already in use come first; only without any is the person asked.
        let mut settings = settings;
        self.migrate_names(&mut settings);
        let named = !settings.display_name.is_empty();
        self.voice.audio.set_settings(settings);
        self.publish_settings();
        !named
    }
}

/// The steps, over everything else while they show. Returns whether they did.
pub fn show(ui: &mut Ui, home: &mut Home) -> bool {
    if matches!(home.welcome, Step::Idle) {
        if !home.welcome_due() {
            return false;
        }
        home.welcome = Step::Name(String::new());
    }
    let mut next = None;
    // The bio goes back to the name; the name is the first step here.
    let back = matches!(home.welcome, Step::Bio(_)).then(|| t!("common.back"));
    let id = matches!(home.welcome, Step::Bio(_));
    let (back, ()) = ui::onboarding(ui, ("welcome", id), back, |ui| match &mut home.welcome {
        Step::Idle => {}
        Step::Name(name) => {
            ui::step_title(ui, t!("welcome.name_title"), t!("welcome.name_help"));
            let label = ui.label(t!("settings.name"));
            let field = ui
                .add(
                    ui::field(name)
                        .char_limit(NAME_CHARS)
                        .hint_text(t!("home.name_hint"))
                        .desired_width(f32::INFINITY),
                )
                .labelled_by(label.id);
            let entered = submitted(ui, &field);
            ui::focus_when_none(ui, &field);
            let empty = name.trim().is_empty();
            let problem = crate::profile::name_problem(name);
            if empty && !name.is_empty() {
                error_text(ui, Some(t!("welcome.name_needed")));
            }
            error_text(ui, problem.as_deref());
            ui.add_space(ui::SPACE_M);
            let ok = !empty && problem.is_none();
            let go = ui.add_enabled_ui(ok, |ui| primary_large(ui, t!("home.continue")));
            if ok && (go.inner.clicked() || entered) {
                next = Some(Some(name.trim().to_owned()));
            }
        }
        Step::Bio(bio) => {
            ui::step_title(ui, t!("welcome.bio_title"), t!("welcome.bio_help"));
            let label = ui.label(t!("settings.bio"));
            ui.add(
                ui::text_area(bio)
                    .hint_text(t!("settings.bio_hint"))
                    .desired_rows(4)
                    .desired_width(f32::INFINITY),
            )
            .labelled_by(label.id);
            let fitted = fit_bio(bio);
            if fitted != *bio {
                *bio = fitted;
            }
            ui.label(ui::caption(format!("{}/{BIO_CHARS}", bio.chars().count())));
            ui.add_space(ui::SPACE_M);
            let save = primary_large(ui, t!("common.save"));
            let skip = secondary_large(ui, t!("welcome.skip"));
            if save.clicked() {
                next = Some(Some(clean_bio(bio)));
            }
            if skip.clicked() {
                next = Some(None);
            }
        }
    });
    match (std::mem::take(&mut home.welcome), next) {
        // The saved name, to change it.
        (Step::Bio(_), None) if back => {
            home.welcome = Step::Name(home.voice.audio.settings().display_name);
        }
        (step, None) => home.welcome = step,
        // The name is saved at once, so it counts even if the bio is never answered.
        (Step::Name(_), Some(Some(name))) => {
            home.change_profile(|settings, servers| {
                (settings.set_default_name(&name, servers.iter()).into_iter())
                    .map(|server| (server, Command::SetDisplayName(name.clone())))
                    .collect()
            });
            home.welcome = Step::Bio(String::new());
        }
        (Step::Bio(_), Some(Some(bio))) if !bio.is_empty() => {
            home.change_profile(|settings, servers| {
                settings.set_bio(&bio, servers.iter());
                (servers.into_iter())
                    .map(|server| (server, Command::SetBio(bio.clone())))
                    .collect()
            });
            home.new_account = false;
        }
        _ => home.new_account = false,
    }
    true
}

#[cfg(test)]
mod tests {
    use eframe::egui::Vec2;
    use egui_kittest::{Harness, kittest::Queryable};

    use super::*;

    fn harness(setup: impl FnOnce(&mut Home)) -> Harness<'static, Option<Home>> {
        let mut harness = Harness::builder()
            .with_size(Vec2::new(700.0, 600.0))
            .build_ui_state(
                |ui, home: &mut Option<Home>| {
                    if let Some(home) = home {
                        show(ui, home);
                    }
                },
                None,
            );
        ui::install(&harness.ctx);
        let (_, identity) = rusttalk_core::identity::DeviceIdentity::create("Desktop").unwrap();
        let data = tempfile::tempdir().unwrap().keep();
        let mut home = Home::new(identity, &harness.ctx, data).unwrap();
        setup(&mut home);
        *harness.state_mut() = Some(home);
        harness.run_steps(3);
        harness
    }

    fn asking(harness: &Harness<'static, Option<Home>>) -> bool {
        harness.query_by_label(t!("welcome.name_title")).is_some()
    }

    #[test]
    fn a_new_account_names_itself_then_may_skip_the_bio() {
        let mut harness = harness(|home| home.new_account = true);
        assert!(asking(&harness));
        // Nothing typed: Continue does nothing.
        harness.get_by_label(t!("home.continue")).click_accesskit();
        harness.run_steps(3);
        assert!(asking(&harness));
        harness
            .get_by_label(t!("settings.name"))
            .type_text("  Ann  ");
        harness.run_steps(2);
        harness.get_by_label(t!("home.continue")).click_accesskit();
        harness.run_steps(3);
        let home = harness.state().as_ref().unwrap();
        assert_eq!(home.voice.audio.settings().display_name, "Ann");
        assert!(harness.query_by_label(t!("welcome.bio_title")).is_some());
        // Back to the name, which can still change.
        harness.get_by_label(t!("common.back")).click_accesskit();
        harness.run_steps(3);
        assert!(asking(&harness));
        harness.get_by_label(t!("home.continue")).click_accesskit();
        harness.run_steps(3);
        harness.get_by_label(t!("welcome.skip")).click_accesskit();
        harness.run_steps(3);
        let home = harness.state().as_ref().unwrap();
        assert!(matches!(home.welcome, Step::Idle) && !home.new_account);
        assert_eq!(home.voice.audio.settings().bio, "");
        assert!(!asking(&harness));
    }

    #[test]
    fn a_restored_device_with_a_synced_name_is_not_asked() {
        let harness = harness(|home| {
            home.restored = true;
            home.synced_once = true;
            home.view = View::Join;
            let mut settings = home.voice.audio.settings();
            settings.display_name = "Ann".into();
            home.voice.audio.set_settings(settings);
        });
        assert!(!asking(&harness));
    }

    #[test]
    fn a_restored_device_without_a_name_is_asked_once_synced() {
        let mut harness = harness(|home| {
            home.restored = true;
            home.view = View::Join;
        });
        // Before the account data arrives from a server, there is nothing to go by.
        assert!(!asking(&harness));
        harness.state_mut().as_mut().unwrap().synced_once = true;
        harness.run_steps(3);
        assert!(asking(&harness));
    }

    #[test]
    fn only_people_without_a_default_name_are_asked() {
        assert!(due(false, true, false, false, false));
        assert!(!due(true, true, true, true, true));
        // Restored or upgraded: after a sync, once servers loaded or on the join page.
        assert!(!due(false, false, false, true, true));
        assert!(!due(false, false, true, false, false));
        assert!(due(false, false, true, true, false));
        assert!(due(false, false, true, false, true));
    }
}
