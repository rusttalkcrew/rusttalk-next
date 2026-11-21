//! The audio mixer for one person in a call: their voice and each app they share,
//! each with a volume and mute, and a link that moves all their apps together.
//! Volumes are kept per person and app name on this device, so an app comes back at
//! the same volume next time. The masters in Settings → Voice & audio scale them.

use std::collections::BTreeMap;

use eframe::egui::{Align, Label, Layout, Slider, Ui, Vec2, WidgetInfo, accesskit::Role};
use livekit::prelude::TrackSid;
use serde::{Deserialize, Serialize};

use crate::{
    audio::{Audio, StreamId},
    server::Id,
    ui::{self, Look},
};

/// The loudest a stream plays, also with the link slider up.
pub const MAX: f32 = 2.0;
const MUTE: Vec2 = Vec2::new(26.0, 22.0);

/// One row's own volume (1.0 is normal) and mute.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Level {
    pub volume: f32,
    pub muted: bool,
}

impl Default for Level {
    fn default() -> Self {
        Self {
            volume: 1.0,
            muted: false,
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct PersonMix {
    pub voice: Level,
    /// While linked, the one slider for all their apps. It scales each app's own
    /// volume, so their balance stays.
    pub link: Option<f32>,
    /// By app name, only those changed from normal.
    pub apps: BTreeMap<String, Level>,
}

impl PersonMix {
    /// Link or unlink. Unlinking moves the link into each of `apps`, so nothing jumps.
    pub fn set_link(&mut self, on: bool, apps: &[&str]) {
        match (self.link, on) {
            (None, true) => self.link = Some(1.0),
            (Some(link), false) => {
                for app in apps {
                    self.apps.entry((*app).to_owned()).or_default();
                }
                for level in self.apps.values_mut() {
                    level.volume = (level.volume * link).min(MAX);
                }
                self.link = None;
            }
            _ => {}
        }
    }

    fn prune(&mut self) {
        self.apps.retain(|_, level| *level != Level::default());
    }
}

/// Every volume and mute of the people you hear, and the masters. This device only.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Mix {
    /// Masters, up to 100%: everyone's voices, and shared apps and stream sound.
    pub voices: f32,
    pub shared: f32,
    /// By person ID in hex, only people with a change.
    pub people: BTreeMap<String, PersonMix>,
}

impl Default for Mix {
    fn default() -> Self {
        Self {
            voices: 1.0,
            shared: 1.0,
            people: BTreeMap::new(),
        }
    }
}

impl Mix {
    pub fn person(&self, person: &Id) -> PersonMix {
        let mut mix = (self.people.get(&hex::encode(person)).cloned()).unwrap_or_default();
        mix.prune();
        mix
    }

    pub fn set_person(&mut self, person: &Id, mut mix: PersonMix) {
        mix.prune();
        let key = hex::encode(person);
        if mix == PersonMix::default() {
            self.people.remove(&key);
        } else {
            self.people.insert(key, mix);
        }
    }

    /// The stream's own row.
    pub fn level(&self, (person, app): &StreamId) -> Level {
        let Some(mix) = self.people.get(&hex::encode(person)) else {
            return Level::default();
        };
        match app {
            None => mix.voice,
            Some(app) => mix.apps.get(app).copied().unwrap_or_default(),
        }
    }

    /// What a stream plays at: master × link × its own volume, never above `MAX`.
    pub fn gain(&self, stream: &StreamId) -> f32 {
        let level = self.level(stream);
        if level.muted {
            return 0.0;
        }
        let volume = level.volume.clamp(0.0, MAX);
        match &stream.1 {
            None => self.voices.clamp(0.0, 1.0) * volume,
            Some(_) => {
                let link = (self.people.get(&hex::encode(stream.0)))
                    .and_then(|mix| mix.link)
                    .unwrap_or(1.0)
                    .clamp(0.0, MAX);
                self.shared.clamp(0.0, 1.0) * (link * volume).min(MAX)
            }
        }
    }
}

/// The mixer for `person` in a menu: their voice when `voice`, then each app they share.
pub fn show(m: &mut ui::Menu, audio: &Audio, person: Id, voice: bool, apps: &[(TrackSid, String)]) {
    let mut names: Vec<&str> = Vec::new();
    for (_, app) in apps {
        if !names.contains(&app.as_str()) {
            names.push(app);
        }
    }
    let mut all = audio.mix();
    let mut mix = all.person(&person);
    let before = mix.clone();
    m.keep_open();
    m.ui().scope(|ui| {
        ui.spacing_mut().item_spacing.y = 2.0;
        if voice {
            let level = &mut mix.voice;
            row(
                ui,
                t!("call.voice"),
                Some(&mut level.volume),
                Some(&mut level.muted),
            );
        }
        if names.len() > 1 {
            ui.horizontal(|ui| {
                ui.add_space(ui::SPACE_S);
                ui::caption_label(ui, t!("call.apps"));
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    ui.add_space(ui::SPACE_S);
                    let mut on = mix.link.is_some();
                    if ui::toggle(ui, &mut on, t!("call.link")).changed() {
                        mix.set_link(on, &names);
                    }
                    ui.label(ui::caption(t!("call.link")));
                });
            });
            if let Some(link) = &mut mix.link {
                row(ui, t!("call.all_apps"), Some(link), None);
            }
        }
        let linked = mix.link.is_some();
        for name in &names {
            let level = mix.apps.entry((*name).to_owned()).or_default();
            let volume = (!linked).then_some(&mut level.volume);
            row(ui, name, volume, Some(&mut level.muted));
        }
    });
    mix.prune();
    if mix != before {
        all.set_person(&person, mix);
        audio.set_mix(all);
    }
}

/// A name with its mute button, and the volume under it.
fn row(ui: &mut Ui, label: &str, volume: Option<&mut f32>, muted: Option<&mut bool>) {
    let off = muted.as_deref().copied().unwrap_or(false);
    ui.horizontal(|ui| {
        ui.add_space(ui::SPACE_S);
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            ui.add_space(ui::SPACE_S);
            if let Some(muted) = muted {
                let (look, tip) = if *muted {
                    (Look::Struck, t!("call.unmute"))
                } else {
                    (Look::Ghost, t!("call.mute"))
                };
                if ui::icon_button(ui, "🔊", tip, look, MUTE).clicked() {
                    *muted = !*muted;
                }
            }
            ui.with_layout(Layout::left_to_right(Align::Center), |ui| {
                ui.add(Label::new(ui::caption(label)).truncate());
            });
        });
    });
    if let Some(volume) = volume {
        ui.horizontal(|ui| {
            ui.add_space(ui::SPACE_S);
            ui.spacing_mut().slider_width = (ui.available_width() - 56.0).max(40.0);
            let slider = ui.add_enabled(!off, percent(volume, MAX));
            slider.widget_info(|| WidgetInfo::labeled(Role::Slider, !off, label));
        });
    }
}

/// A slider from 0 to `max`, shown in percent.
pub fn percent(value: &mut f32, max: f32) -> Slider<'_> {
    Slider::new(value, 0.0..=max)
        .custom_formatter(|value, _| format!("{:.0}%", value * 100.0))
        .custom_parser(|text| {
            let percent: f64 = text.trim().trim_end_matches('%').parse().ok()?;
            Some(percent / 100.0)
        })
}

/// Settings → Voice & audio: the masters. App sounds is the call sounds' own volume.
pub fn masters(ui: &mut Ui, settings: &mut crate::audio::Settings) {
    ui::page_section(ui, t!("call.volume"));
    let rows = [
        (t!("call.voices"), &mut settings.mix.voices),
        (t!("call.shared_audio"), &mut settings.mix.shared),
        (t!("call.app_sounds"), &mut settings.call_sounds.volume),
    ];
    for (label, value) in rows {
        ui::setting_row(ui, label, "", |ui| {
            // The same width for "90%" and "100%", so the sliders line up.
            let size = Vec2::new(
                ui.spacing().slider_width + 80.0,
                ui.spacing().interact_size.y,
            );
            ui.allocate_ui_with_layout(size, Layout::left_to_right(Align::Center), |ui| {
                ui.set_min_width(size.x);
                ui.add(percent(value, 1.0))
                    .widget_info(|| WidgetInfo::labeled(Role::Slider, true, label));
            });
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ANN: Id = [7; 32];

    fn app(name: &str) -> StreamId {
        (ANN, Some(name.into()))
    }

    fn close(a: f32, b: f32) -> bool {
        (a - b).abs() < 1e-5
    }

    #[test]
    fn gain_is_master_times_link_times_row_and_never_above_the_max() {
        let mut mix = Mix {
            voices: 0.5,
            shared: 0.8,
            ..Mix::default()
        };
        let mut ann = PersonMix {
            voice: Level {
                volume: 1.5,
                muted: false,
            },
            link: Some(0.5),
            ..PersonMix::default()
        };
        ann.apps.insert(
            "Spotify".into(),
            Level {
                volume: 1.2,
                muted: false,
            },
        );
        mix.set_person(&ANN, ann.clone());
        assert!(close(mix.gain(&(ANN, None)), 0.75));
        assert!(close(mix.gain(&app("Spotify")), 0.8 * 0.5 * 1.2));
        // An app never changed plays at the link alone.
        assert!(close(mix.gain(&app("Game")), 0.8 * 0.5));
        // Someone never changed plays at the masters.
        assert!(close(mix.gain(&([9; 32], None)), 0.5));

        // Link and row both up: capped at 200%, before the master.
        ann.link = Some(2.0);
        mix.set_person(&ANN, ann.clone());
        assert!(close(mix.gain(&app("Spotify")), 0.8 * MAX));

        ann.apps.get_mut("Spotify").unwrap().muted = true;
        ann.voice.muted = true;
        mix.set_person(&ANN, ann);
        assert_eq!(mix.gain(&app("Spotify")), 0.0);
        assert_eq!(mix.gain(&(ANN, None)), 0.0);
    }

    #[test]
    fn the_link_keeps_the_balance_and_unlinking_keeps_the_sound() {
        let mut ann = PersonMix::default();
        ann.apps.insert(
            "Spotify".into(),
            Level {
                volume: 0.5,
                muted: false,
            },
        );
        ann.set_link(true, &["Spotify", "Game"]);
        assert_eq!(ann.link, Some(1.0));
        ann.link = Some(1.6);
        let mut mix = Mix::default();
        mix.set_person(&ANN, ann.clone());
        let (spotify, game) = (mix.gain(&app("Spotify")), mix.gain(&app("Game")));
        assert!(close(spotify / game, 0.5));

        ann.set_link(false, &["Spotify", "Game"]);
        assert_eq!(ann.link, None);
        mix.set_person(&ANN, ann.clone());
        assert!(close(mix.gain(&app("Spotify")), spotify));
        assert!(close(mix.gain(&app("Game")), game));
        assert!(close(ann.apps["Game"].volume, 1.6));
    }

    #[test]
    fn volumes_survive_a_restart_and_unchanged_people_are_not_kept() {
        let mut settings = crate::audio::Settings::default();
        settings.mix.shared = 0.6;
        let mut ann = PersonMix::default();
        ann.voice.volume = 1.4;
        ann.apps.insert(
            "All system audio".into(),
            Level {
                volume: 0.3,
                muted: true,
            },
        );
        ann.apps.insert("Game".into(), Level::default());
        settings.mix.set_person(&ANN, ann);
        settings.mix.set_person(&[9; 32], PersonMix::default());
        assert_eq!(settings.mix.people.len(), 1);

        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("voice.json");
        rusttalk_core::client::save_json(dir.path(), &file, &settings).unwrap();
        let loaded: crate::audio::Settings = crate::load_json(&file).unwrap();
        assert!(loaded == settings);
        let ann = loaded.mix.person(&ANN);
        assert_eq!(ann.voice.volume, 1.4);
        assert_eq!(ann.apps.len(), 1);
        assert!(ann.apps["All system audio"].muted);

        // Voice settings saved before the mixer load with normal volumes.
        std::fs::write(&file, r#"{"push_to_talk":true}"#).unwrap();
        let old: crate::audio::Settings = crate::load_json(&file).unwrap();
        assert!(old.mix == Mix::default());
    }
}
