//! Sensitive pictures (#682): marked and detected pictures show blurred until tapped,
//! in a message, an album and the viewer; the settings decide what blurs; the composer
//! marks a message's pictures. Detection here is a fake that flags magenta pictures;
//! every picture is self-made.

use super::gallery::{NOW, msg, photo};
use super::*;
use crate::{net::Fetch, sensitive::Prefs};
use egui::{Color32, ColorImage};
use egui_kittest::{Harness, kittest::Queryable};
use rusttalk_core::client::{Attachment, decode_frames, prepare_file};

type Screen = (Home, crate::update::Updater, crate::keystore::Keystore);

/// "Sensitive" for a magenta picture, "not sensitive" for anything else.
fn fake(image: &ColorImage) -> anyhow::Result<f32> {
    Ok(if image.pixels[0] == Color32::from_rgb(255, 0, 255) {
        1.0
    } else {
        0.0
    })
}

fn magenta() -> Vec<u8> {
    let image = image::RgbaImage::from_pixel(240, 160, image::Rgba([255, 0, 255, 255]));
    let mut png = std::io::Cursor::new(Vec::new());
    image.write_to(&mut png, image::ImageFormat::Png).unwrap();
    png.into_inner()
}

fn hidden(name: &str) -> String {
    format!("{name} · {}", t!("sensitive.tap_to_show"))
}

struct Fixture {
    h: Harness<'static, Option<Screen>>,
    sent: tokio::sync::mpsc::UnboundedReceiver<Command>,
    blobs: Vec<(Id, Vec<u8>)>,
    address: url::Url,
}

/// The whole screen in the projects channel: an ordinary landscape, a marked beach,
/// a magenta picture the fake detector flags, and an album of a marked and an ordinary
/// picture.
fn fixture(size: Vec2, dark: bool, prefs: Prefs) -> Fixture {
    let (mut base, sent) = files_harness(dark);
    let mut home = base.state_mut().take().unwrap();
    home.view = View::Server(0);
    home.servers[0].open = Some(PROJECTS);
    home.layout.list = false;
    home.find.opened = false;
    let mut notify = home.notify.prefs().clone();
    notify.desktop = false;
    home.notify.set_prefs(notify);
    let mut settings = home.voice.audio.settings();
    settings.sensitive = prefs;
    home.voice.audio.set_settings(settings);
    home.files.sensitive.set_detector(fake);
    let address = home.servers[0].address.clone();
    let mut blobs = Vec::new();
    let mut file = |name: &str, bytes: Vec<u8>, mark: bool| {
        let (mut file, thumb) = prepare_file(name, "image/png", &bytes, &bytes).unwrap();
        if mark {
            assert!(file.mark_sensitive());
        }
        blobs.push((file.thumbnail.as_ref().unwrap().id, thumb.unwrap()));
        blobs.push((file.file.id, bytes));
        file
    };
    let files: Vec<Vec<Attachment>> = vec![
        vec![file("landscape.png", photo(240, 160, 2), false)],
        vec![file("beach.png", photo(240, 160, 5), true)],
        vec![file("flagged.png", magenta(), false)],
        vec![
            file("album 1.png", photo(300, 300, 7), true),
            file("album 2.png", photo(300, 300, 3), false),
        ],
    ];
    let chat = home.servers[0].chats.get_mut(&PROJECTS).unwrap();
    chat.messages = (files.into_iter().enumerate())
        .map(|(n, files)| {
            msg(
                40 + n as u8,
                ANN,
                NOW - 600 * (4 - n as i64),
                files,
                &address,
            )
        })
        .collect();
    chat.at_start = true;
    chat.loading = false;
    let data = tempfile::tempdir().unwrap();
    let mut h = Harness::builder()
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
    *h.state_mut() = Some((
        home,
        crate::update::Updater::new(data.path()),
        crate::keystore::without_keychain(data.path()),
    ));
    h.run_steps(4);
    let mut f = Fixture {
        h,
        sent,
        blobs,
        address,
    };
    f.answer();
    f
}

impl Fixture {
    fn home(&mut self) -> &mut Home {
        &mut self.h.state_mut().as_mut().unwrap().0
    }

    /// Answer every picture request so far; returns what was asked for.
    fn answer(&mut self) -> Vec<(Id, Fetch)> {
        let mut asked = Vec::new();
        for _ in 0..3 {
            let commands: Vec<_> = std::iter::from_fn(|| self.sent.try_recv().ok()).collect();
            for command in commands {
                let Command::Image(blob, fetch @ (Fetch::Thumbnail | Fetch::Full | Fetch::Inline)) =
                    command
                else {
                    continue;
                };
                asked.push((blob.id, fetch));
                let bytes = &self.blobs.iter().find(|(id, _)| *id == blob.id).unwrap().1;
                let fit = (fetch == Fetch::Inline).then_some(crate::files::THUMBNAIL as u32);
                let frames = decode_frames(bytes, fit, 64 * 1024 * 1024).unwrap();
                let frames = frames.into_iter().map(crate::net::frame_image).collect();
                let update = Update::Image(self.address.clone(), blob, fetch, Ok(frames));
                let ctx = self.h.ctx.clone();
                self.home().apply(&ctx, update, 0.0);
            }
            self.h.run_steps(3);
        }
        asked
    }

    fn shows(&self, label: &str) -> bool {
        self.h.query_all_by_label(label).next().is_some()
    }

    fn count(&self, label: &str) -> usize {
        self.h.query_all_by_label(label).count()
    }

    fn set(&mut self, prefs: Prefs) {
        let audio = self.home().voice.audio.clone();
        let mut settings = audio.settings();
        settings.sensitive = prefs;
        audio.set_settings(settings);
        self.h.run_steps(3);
    }

    fn screenshot(&mut self, name: &str, size: Vec2, dark: bool) {
        if let Ok(dir) = std::env::var("RUSTTALK_TEST_SCREENSHOTS") {
            std::fs::create_dir_all(&dir).unwrap();
            let theme = if dark { "dark" } else { "light" };
            let path = format!("{dir}/{name}-{theme}-{}x{}.png", size.x, size.y);
            self.h.render().unwrap().save(path).unwrap();
        }
    }
}

const DESKTOP: Vec2 = Vec2::new(1100.0, 900.0);

#[test]
fn marked_and_detected_pictures_blur_until_tapped() {
    let mut f = fixture(DESKTOP, true, Prefs::default());
    for name in ["beach.png", "flagged.png", "album 1.png"] {
        assert!(f.shows(&hidden(name)), "{name} is blurred");
    }
    for name in ["landscape.png", "album 2.png"] {
        assert!(!f.shows(&hidden(name)), "{name} is not blurred");
    }
    // A tap shows the picture where it is, without opening the viewer.
    f.h.get_by_label(&hidden("beach.png")).click();
    f.h.run_steps(3);
    assert!(!f.shows(&hidden("beach.png")));
    f.h.get_by_label(&hidden("album 1.png")).click();
    f.h.run_steps(3);
    assert!(!f.shows(&hidden("album 1.png")));
    let asked = f.answer();
    assert!(
        asked.iter().all(|(_, fetch)| *fetch != Fetch::Full),
        "{asked:?}"
    );
    assert!(
        f.shows(&hidden("flagged.png")),
        "other pictures stay blurred"
    );
}

#[test]
fn the_settings_decide_what_blurs() {
    let mut f = fixture(DESKTOP, true, Prefs::default());
    f.set(Prefs {
        blur_marked: false,
        ..Prefs::default()
    });
    assert!(!f.shows(&hidden("beach.png")));
    assert!(f.shows(&hidden("flagged.png")));
    f.set(Prefs {
        detect: false,
        ..Prefs::default()
    });
    assert!(f.shows(&hidden("beach.png")));
    assert!(!f.shows(&hidden("flagged.png")));
    f.set(Prefs {
        show_all: true,
        ..Prefs::default()
    });
    for name in ["beach.png", "flagged.png", "album 1.png"] {
        assert!(!f.shows(&hidden(name)), "{name}");
    }
}

#[test]
fn the_viewer_blurs_a_marked_picture_until_tapped() {
    let mut f = fixture(DESKTOP, true, Prefs::default());
    f.h.get_by_label("album 2.png").click();
    f.h.run_steps(3);
    f.answer();
    f.h.key_press(egui::Key::ArrowLeft);
    f.h.run_steps(3);
    f.answer();
    // The album cell behind it and the picture in the viewer.
    assert_eq!(f.count(&hidden("album 1.png")), 2);
    let label = hidden("album 1.png");
    f.h.query_all_by_label(&label).last().unwrap().click();
    f.h.run_steps(3);
    assert_eq!(f.count(&hidden("album 1.png")), 0);
}

#[test]
fn an_agreed_nsfw_channel_shows_marked_pictures_when_chosen() {
    let mut f = fixture(DESKTOP, true, Prefs::default());
    let address = f.address.clone();
    let home = f.home();
    // A harness-free fixture writes nowhere: answers stay in memory.
    home.nsfw_agreed = crate::home::nsfw::Agreed::default();
    home.nsfw_agreed.agree(&address, PROJECTS);
    let channel = (home.servers[0].dir.categories.iter_mut())
        .flat_map(|category| &mut category.channels)
        .find(|channel| channel.id == PROJECTS)
        .unwrap();
    channel.nsfw = true;
    f.h.run_steps(3);
    assert!(f.shows(&hidden("beach.png")), "still blurred by default");
    f.set(Prefs {
        nsfw_unblurred: true,
        ..Prefs::default()
    });
    assert!(!f.shows(&hidden("beach.png")));
    assert!(!f.shows(&hidden("album 1.png")));
    // Only in NSFW channels.
    let channel = (f.home().servers[0].dir.categories.iter_mut())
        .flat_map(|category| &mut category.channels)
        .find(|channel| channel.id == PROJECTS)
        .unwrap();
    channel.nsfw = false;
    f.h.run_steps(3);
    assert!(f.shows(&hidden("beach.png")));
}

#[test]
fn a_picture_can_be_blurred_and_shown_from_its_menu() {
    let mut f = fixture(DESKTOP, true, Prefs::default());
    let landscape = f.home().servers[0].chats[&PROJECTS].messages[0].files[0].clone();
    let beach = f.home().servers[0].chats[&PROJECTS].messages[1].files[0].clone();
    f.home().files.sensitive.choose(&landscape, true);
    f.home().files.sensitive.choose(&beach, false);
    f.h.run_steps(3);
    assert!(f.shows(&hidden("landscape.png")));
    assert!(!f.shows(&hidden("beach.png")));
}

#[test]
fn the_composer_marks_a_messages_pictures() {
    let data = tempfile::tempdir().unwrap();
    let path = data.path().join("holiday.png");
    std::fs::write(&path, photo(64, 48, 1)).unwrap();
    let mut f = fixture(DESKTOP, true, Prefs::default());
    f.home().servers[0]
        .chats
        .get_mut(&PROJECTS)
        .unwrap()
        .staged
        .push(crate::server::Staged {
            path,
            name: "holiday.png".into(),
            size: 100,
        });
    f.h.run_steps(3);
    f.h.get_by_label(t!("sensitive.mark")).click_accesskit();
    f.h.run_steps(2);
    f.h.get_by_label(t!("home.send")).click_accesskit();
    f.h.run_steps(3);
    let drafts: Vec<_> = std::iter::from_fn(|| f.sent.try_recv().ok())
        .filter_map(|command| match command {
            Command::Send(draft) => Some(draft),
            _ => None,
        })
        .collect();
    assert_eq!(drafts.len(), 1);
    assert!(drafts[0].sensitive && drafts[0].files.len() == 1);
    let chat = &f.home().servers[0].chats[&PROJECTS];
    assert!(!chat.mark_sensitive, "the next message starts unmarked");
}

#[test]
fn only_pictures_and_videos_offer_the_mark() {
    let data = tempfile::tempdir().unwrap();
    let path = data.path().join("notes.pdf");
    std::fs::write(&path, b"%PDF").unwrap();
    let mut f = fixture(DESKTOP, true, Prefs::default());
    let chat = f.home().servers[0].chats.get_mut(&PROJECTS).unwrap();
    chat.staged.push(crate::server::Staged {
        path,
        name: "notes.pdf".into(),
        size: 4,
    });
    chat.mark_sensitive = true;
    f.h.run_steps(3);
    assert!(!f.shows(t!("sensitive.mark")));
    assert!(!f.home().servers[0].chats[&PROJECTS].mark_sensitive);
}

#[test]
fn privacy_settings_have_sensitive_media() {
    let mut f = fixture(Vec2::new(1100.0, 700.0), true, Prefs::default());
    f.home().settings = Some(Tab::Privacy);
    f.h.run_steps(4);
    for label in [
        t!("sensitive.blur_marked"),
        t!("sensitive.detect"),
        t!("sensitive.nsfw"),
        t!("sensitive.show_all"),
    ] {
        assert!(f.shows(label), "{label}");
    }
}

#[test]
fn sensitive_screenshots() {
    for size in [Vec2::new(1100.0, 700.0), Vec2::new(390.0, 844.0)] {
        for dark in [true, false] {
            let mut f = fixture(size, dark, Prefs::default());
            f.screenshot("sensitive-chat", size, dark);
            let data = tempfile::tempdir().unwrap();
            let path = data.path().join("holiday.png");
            std::fs::write(&path, photo(240, 160, 4)).unwrap();
            let chat = f.home().servers[0].chats.get_mut(&PROJECTS).unwrap();
            chat.staged.push(crate::server::Staged {
                path,
                name: "holiday.png".into(),
                size: 100,
            });
            chat.mark_sensitive = true;
            f.h.run_steps(6);
            f.screenshot("sensitive-composer", size, dark);
            f.home().settings = Some(Tab::Privacy);
            f.h.run_steps(6);
            // Its entry in the page's outline scrolls to it.
            if let Some(entry) = f.h.query_all_by_label(t!("sensitive.section")).next() {
                entry.click();
            }
            f.h.run_steps(10);
            f.screenshot("sensitive-settings", size, dark);
        }
    }
}
