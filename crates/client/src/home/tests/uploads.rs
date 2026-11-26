use super::*;
use egui_kittest::{
    Harness,
    kittest::{NodeT, Queryable},
};
use std::{path::PathBuf, sync::Mutex};

#[derive(Debug)]
struct DropPath(PathBuf);
impl egui::DroppedFile for DropPath {
    fn path(&self) -> &std::path::Path {
        &self.0
    }
    fn bytes(&self) -> Result<Vec<u8>, String> {
        panic!("size check must not read the payload")
    }
}

pub(super) struct ResetPicker;
impl Drop for ResetPicker {
    fn drop(&mut self) {
        *files::TEST_ANSWER.lock().unwrap() = None;
    }
}

static PICKED: Mutex<Vec<PathBuf>> = Mutex::new(Vec::new());
pub(super) static PICKER_TEST: Mutex<()> = Mutex::new(());

#[test]
fn oversized_stored_picker_feedback() {
    let _picker = PICKER_TEST.lock().unwrap();
    let _reset = ResetPicker;
    let data = tempfile::tempdir().unwrap();
    let large = data.path().join("sample.apk");
    std::fs::File::create(&large)
        .unwrap()
        .set_len(177 * files::MIB)
        .unwrap();
    let small = data.path().join("control.txt");
    std::fs::write(&small, b"synthetic control").unwrap();
    *files::TEST_ANSWER.lock().unwrap() = Some(|_| PICKED.lock().unwrap().clone());
    for dark in [true, false] {
        for scene in [
            "chat",
            "voice-empty",
            "voice-split",
            "voice-side",
            "dm",
            "shared-chat",
            "shared-voice",
            "shared-voice-split",
        ] {
            let shared = scene.starts_with("shared");
            let voice = scene.contains("voice");
            let (mut base, mut sent) = files_harness(dark);
            let mut home = base.state_mut().take().unwrap();
            home.view = View::Server(0);
            home.servers[0].open = Some(PROJECTS);
            home.servers[0].dir.categories[0].channels[0].voice = voice;
            if scene.ends_with("split") || scene == "voice-side" {
                home.servers[0].voice.insert(
                    PROJECTS,
                    vec![crate::server::Peer {
                        person: ANN,
                        connecting: false,
                        unknown: false,
                        status: rusttalk_core::client::voice::VoiceStatus {
                            muted: false,
                            deafened: false,
                        },
                        apps: vec![],
                        screens: vec![],
                        camera: None,
                        terminal: None,
                        diagram: None,
                    }],
                );
            }
            if scene == "voice-side" {
                home.layout.split.side_by_side = true;
            }
            if scene == "dm" {
                home.servers[0].dir.dms.push(crate::server::Dm {
                    channel: PROJECTS,
                    peer: ANN,
                });
                home.view = View::Direct(Some(ANN));
            }
            home.find.opened = false;
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
            let mut h = egui_kittest::Harness::builder()
                .with_size(Vec2::new(1100.0, 700.0))
                .build_ui_state(
                    |ui, screen: &mut Option<Screen>| {
                        if let Some((home, updates, keys)) = screen {
                            show(ui, home, updates, keys);
                        }
                    },
                    None,
                );
            ui::install(&h.ctx);
            h.ctx
                .data_mut(|d| d.insert_temp(files_id(PROJECTS), shared));
            *h.state_mut() = Some((
                home,
                crate::update::Updater::new(data.path()),
                crate::keystore::without_keychain(data.path()),
            ));
            h.run_steps(4);
            let ctx = h.ctx.clone();
            let home = &mut h.state_mut().as_mut().unwrap().0;
            *PICKED.lock().unwrap() = vec![large.clone(), small.clone()];
            let ask = if shared {
                Ask::Share(home.servers[0].address.clone(), PROJECTS)
            } else if scene == "dm" {
                Ask::AttachDm(ANN)
            } else {
                Ask::Attach(home.servers[0].address.clone(), PROJECTS)
            };
            home.files.ask(&ctx, ask);
            // The picker answers on another thread: wait for it by time, not a loop count.
            let deadline = std::time::Instant::now() + Duration::from_secs(10);
            while std::time::Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(5));
                h.run_steps(1);
                if shared {
                    while let Ok(command) = sent.try_recv() {
                        if let Command::ShareFiles(channel, paths) = command {
                            assert_eq!(channel, PROJECTS);
                            assert_eq!(paths, vec![large.clone(), small.clone()]);
                            // net::files tests exercise the read/size mapping; feed its result
                            // back through the same update used by the network worker.
                            let now = h.ctx.input(|i| i.time);
                            h.state_mut().as_mut().unwrap().0.apply(
                                &ctx,
                                Update::Failed(files::too_big("sample.apk", 177 * files::MIB)),
                                now,
                            );
                        }
                    }
                }
                if h.state().as_ref().unwrap().0.toast.is_some() {
                    break;
                }
            }
            h.run_steps(4);
            let home = &h.state().as_ref().unwrap().0;
            let chat = if scene == "dm" {
                &home.threads[&ANN].chat
            } else {
                &home.servers[0].chats[&PROJECTS]
            };
            assert_eq!(chat.staged.len(), usize::from(!shared));
            let text = &home.toast.as_ref().unwrap().0;
            assert_eq!(text, &files::too_big("sample.apk", 177 * files::MIB));
            let rect = h.get_by_label(text).rect();
            assert!(rect.min.x >= 0.0 && rect.max.x <= 1100.0);
            assert!(rect.min.y >= 0.0 && rect.max.y <= 700.0);
            if !shared {
                assert!(
                    rect.min.y > 400.0,
                    "feedback belongs beside the composer: {scene}: {rect:?}"
                );
            }
            h.get_by_label(t!("common.dismiss")).click_accesskit();
            h.run_steps(2);
            assert!(h.state().as_ref().unwrap().0.toast.is_none());
            if !shared {
                let home = &mut h.state_mut().as_mut().unwrap().0;
                let chat = if scene == "dm" {
                    &mut home.threads.get_mut(&ANN).unwrap().chat
                } else {
                    home.servers[0].chats.get_mut(&PROJECTS).unwrap()
                };
                chat.staged.clear();
                h.input_mut().dropped_files = vec![
                    std::sync::Arc::new(DropPath(large.clone())),
                    std::sync::Arc::new(DropPath(small.clone())),
                ];
                h.run_steps(4);
                h.get_by_label(&files::too_big("sample.apk", 177 * files::MIB));
                let home = &h.state().as_ref().unwrap().0;
                let chat = if scene == "dm" {
                    &home.threads[&ANN].chat
                } else {
                    &home.servers[0].chats[&PROJECTS]
                };
                assert_eq!(chat.staged.len(), 1);
                assert_eq!(chat.staged[0].path, small);
                if scene == "chat" {
                    let home = &mut h.state_mut().as_mut().unwrap().0;
                    home.toast = None;
                    *PICKED.lock().unwrap() = vec![large.clone()];
                    home.files
                        .ask(&ctx, Ask::Live(home.servers[0].address.clone(), PROJECTS));
                    let mut offered = false;
                    // The picker answers on another thread: wait for it by time, not a loop count.
                    let deadline = std::time::Instant::now() + Duration::from_secs(10);
                    while std::time::Instant::now() < deadline {
                        std::thread::sleep(Duration::from_millis(5));
                        h.run_steps(1);
                        while let Ok(command) = sent.try_recv() {
                            if let Command::SendLive(channel, path) = command {
                                assert_eq!(channel, PROJECTS);
                                assert_eq!(path, large);
                                offered = true;
                            }
                        }
                        if offered {
                            break;
                        }
                    }
                    assert!(offered, "177 MB remains eligible for live transfer");
                    assert!(h.state().as_ref().unwrap().0.toast.is_none());
                }
            }
        }
    }
}

#[test]
fn live_offers_wait_for_download_in_every_layout() {
    let _route = crate::route::test_lock();
    let _picker = PICKER_TEST.lock().unwrap();
    let _reset = ResetPicker;
    let data = tempfile::tempdir().unwrap();
    let path = data.path().join("sample.txt");
    *PICKED.lock().unwrap() = vec![path.clone()];
    *files::TEST_ANSWER.lock().unwrap() = Some(|name| {
        assert_eq!(name, Some("sample.txt"));
        PICKED.lock().unwrap().clone()
    });
    let transfer = [9; 32];
    for dark in [true, false] {
        for size in [
            Vec2::new(390., 844.),
            Vec2::new(820., 1180.),
            Vec2::new(1100., 700.),
        ] {
            for scene in ["chat", "dm", "forum-post", "forum-reply", "call"] {
                let (mut fixture, mut sent) = files_harness(dark);
                let mut home = fixture.state_mut().take().unwrap();
                let server = &mut home.servers[0];
                let address = server.address.clone();
                home.view = View::Server(0);
                server.open = Some(PROJECTS);
                home.layout.list = scene == "call";
                home.find.opened = false;
                let mut prefs = home.notify.prefs().clone();
                prefs.desktop = false;
                home.notify.set_prefs(prefs);
                let mut settings = home.voice.audio.settings();
                settings.language = "en".into();
                settings.set_appearance(ui::Appearance {
                    mode: if dark {
                        ui::Mode::Dark
                    } else {
                        ui::Mode::Light
                    },
                    ..Default::default()
                });
                home.voice.audio.set_settings(settings);
                let message = Msg {
                    id: [10; 32],
                    person: ANN,
                    at: chrono::Utc::now().timestamp(),
                    body: Body::Text("A live file".into()),
                    backfilled: false,
                    reply_to: None,
                    quoted_author: None,
                    edit_of: None,
                    files: vec![],
                    live: Some((transfer, "sample.txt".into(), 100)),
                    sticker: None,
                    replicas: vec![],
                    copy_files: vec![],
                };
                match scene {
                    "call" => {
                        server.dir.categories[0].channels[0].voice = true;
                        home.voice.call = Some(crate::server::Call {
                            server: address,
                            channel: PROJECTS,
                            state: crate::server::CallState::Connected,
                            stats: Default::default(),
                            can_talk: true,
                            can_share_video: true,
                        });
                        home.apply(
                            &fixture.ctx,
                            Update::LiveOffer(crate::net::LiveOffer {
                                id: transfer,
                                from: Some(ANN),
                                name: "sample.txt".into(),
                                size: 100,
                                chat: false,
                            }),
                            0.0,
                        );
                    }
                    "dm" => {
                        home.view = View::Direct(Some(ANN));
                        server.dir.dms.push(crate::server::Dm {
                            channel: PROJECTS,
                            peer: ANN,
                        });
                        home.threads
                            .entry(ANN)
                            .or_default()
                            .chat
                            .messages
                            .push(Msg {
                                replicas: vec![address],
                                ..message
                            });
                    }
                    _ => {
                        server.dir.categories[0].channels[0].forum = scene.starts_with("forum");
                        let chat = server.chats.get_mut(&PROJECTS).unwrap();
                        if scene == "forum-reply" {
                            let post = [11; 32];
                            chat.messages.push(Msg {
                                id: post,
                                live: None,
                                ..message.clone()
                            });
                            chat.messages.push(Msg {
                                reply_to: Some(post),
                                ..message
                            });
                        } else {
                            chat.messages.push(message);
                        }
                    }
                }
                let mut h = Harness::builder()
                    .with_context_setup(ui::install)
                    .with_size(size)
                    .build_ui_state(
                        |ui, (home, updates, keys): &mut Screen| {
                            show(ui, home, updates, keys);
                        },
                        (
                            home,
                            crate::update::Updater::new(data.path()),
                            crate::keystore::without_keychain(data.path()),
                        ),
                    );
                h.run_steps(4);
                if scene == "forum-reply" {
                    h.get_by_label_contains("1 reply").click_accesskit();
                    h.run_steps(3);
                }
                if let Ok(dir) = std::env::var("RUSTTALK_LIVE_OFFER_SCREENSHOTS") {
                    let theme = if dark { "dark" } else { "light" };
                    h.render()
                        .unwrap()
                        .save(format!("{dir}/{scene}-{}-{theme}.png", size.x))
                        .unwrap();
                }
                let download = h.get_by_role_and_label(egui::Role::Button, "Download");
                assert!(
                    egui::Rect::from_min_size(egui::Pos2::ZERO, size)
                        .contains_rect(download.rect())
                );
                for label in ["Save", "Decline", "Cancel", "Stop sending"] {
                    assert!(
                        h.query_by_role_and_label(egui::Role::Button, label)
                            .is_none()
                    );
                }
                let transfer_commands =
                    |sent: &mut tokio::sync::mpsc::UnboundedReceiver<Command>| {
                        std::iter::from_fn(|| sent.try_recv().ok())
                            .filter(|command| {
                                matches!(command, Command::AcceptFile(..) | Command::CancelFile(_))
                            })
                            .collect::<Vec<_>>()
                    };
                assert!(transfer_commands(&mut sent).is_empty());
                download.click();
                let mut accepted = false;
                // The picker answers on another thread: wait for it by time, not a loop count.
                let deadline = std::time::Instant::now() + Duration::from_secs(10);
                while std::time::Instant::now() < deadline {
                    h.run_steps(1);
                    let commands = transfer_commands(&mut sent);
                    if !commands.is_empty() {
                        assert!(
                            matches!(commands.as_slice(), [Command::AcceptFile(id, dest)] if *id == transfer && *dest == path)
                        );
                        accepted = true;
                        break;
                    }
                    std::thread::sleep(Duration::from_millis(5));
                }
                assert!(accepted, "{scene} at {size:?} did not accept the file");
                let ctx = h.ctx.clone();
                h.state_mut().0.apply(
                    &ctx,
                    Update::LiveProgress(
                        transfer,
                        LiveFileEvent::Progress {
                            bytes: 50,
                            total: 100,
                        },
                    ),
                    0.0,
                );
                h.run_steps(3);
                assert!(h.query_by_label("Download").is_none());
                assert!(
                    h.query_all_by_role(egui::Role::ProgressIndicator)
                        .any(|node| { node.accesskit_node().numeric_value() == Some(50.0) })
                );
                let cancel = h.get_by_role_and_label(egui::Role::Button, "Cancel");
                if dark {
                    cancel.click();
                    h.run_steps(3);
                    assert!(
                        matches!(transfer_commands(&mut sent).as_slice(), [Command::CancelFile(id)] if *id == transfer)
                    );
                } else {
                    h.state_mut().0.apply(
                        &ctx,
                        Update::LiveProgress(transfer, LiveFileEvent::Done),
                        0.0,
                    );
                    h.run_steps(3);
                    if scene == "call" {
                        h.get_by_label("Saved sample.txt");
                        h.get_by_label("Dismiss").click();
                        h.run_steps(3);
                        assert!(h.query_by_label("Saved sample.txt").is_none());
                    } else {
                        h.get_by_label("Saved");
                    }
                    assert!(transfer_commands(&mut sent).is_empty());
                }
            }
        }
    }
}

/// Every upload takes what the picker returns (`TEST_ANSWER` here, the system picker
/// on Android): files to open, never a place to save.
#[test]
fn every_upload_takes_the_picked_files() {
    /// Draw frames until `done`; the picker answers on another thread.
    fn until(
        h: &mut Harness<'_, Option<Screen>>,
        what: &str,
        mut done: impl FnMut(&mut Home) -> bool,
    ) {
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        while std::time::Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(5));
            h.run_steps(1);
            if done(&mut h.state_mut().as_mut().unwrap().0) {
                return;
            }
        }
        panic!("{what} never got the picked file");
    }
    fn ask(h: &mut Harness<'_, Option<Screen>>, ask: Ask) {
        let ctx = h.ctx.clone();
        h.state_mut().as_mut().unwrap().0.files.ask(&ctx, ask);
    }
    /// Draw frames until `label` shows.
    fn shows(h: &mut Harness<'_, Option<Screen>>, label: &str) {
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        while h.query_by_label(label).is_none() && std::time::Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(5));
            h.run_steps(1);
        }
        h.get_by_label(label);
    }

    let _route = crate::route::test_lock();
    let _picker = PICKER_TEST.lock().unwrap();
    let _reset = ResetPicker;
    let data = tempfile::tempdir().unwrap();
    let picture = data.path().join("photo.png");
    image::RgbaImage::from_pixel(64, 64, image::Rgba([40, 180, 90, 255]))
        .save(&picture)
        .unwrap();
    let zip = data.path().join("pack.zip");
    std::fs::write(&zip, b"PK").unwrap();
    *PICKED.lock().unwrap() = vec![picture.clone()];
    *files::TEST_ANSWER.lock().unwrap() = Some(|name| {
        assert_eq!(name, None, "uploads open files");
        PICKED.lock().unwrap().clone()
    });
    let (mut base, mut sent) = files_harness(true);
    let mut home = base.state_mut().take().unwrap();
    home.view = View::Server(0);
    home.servers[0].open = Some(PROJECTS);
    home.servers[0].dir.perms.manage_emoji = true;
    home.servers[0].dir.emoji_max = (50, 20);
    home.servers[0].dir.dms.push(crate::server::Dm {
        channel: PROJECTS,
        peer: ANN,
    });
    let address = home.servers[0].address.clone();
    let mut h = Harness::builder()
        .with_size(Vec2::new(1100.0, 700.0))
        .build_ui_state(
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
        crate::update::Updater::new(data.path()),
        crate::keystore::without_keychain(data.path()),
    ));
    h.run_steps(4);

    let staged = |chat: &Chat| chat.staged.iter().any(|file| file.path == picture);
    for page in [false, true] {
        let next = if page {
            Ask::PagePictures(address.clone(), PROJECTS)
        } else {
            Ask::Attach(address.clone(), PROJECTS)
        };
        ask(&mut h, next);
        until(&mut h, "a channel message", |home| {
            let chat = home.servers[0].chats.get_mut(&PROJECTS).unwrap();
            let done = staged(chat);
            chat.staged.clear();
            done
        });
    }
    ask(&mut h, Ask::AttachDm(ANN));
    until(&mut h, "a DM", |home| {
        (home.threads.get(&ANN)).is_some_and(|thread| staged(&thread.chat))
    });
    while sent.try_recv().is_ok() {}
    for live in [false, true] {
        let next = if live {
            Ask::Live(address.clone(), PROJECTS)
        } else {
            Ask::Share(address.clone(), PROJECTS)
        };
        ask(&mut h, next);
        until(&mut h, "sharing", |_| {
            std::iter::from_fn(|| sent.try_recv().ok()).any(|command| match command {
                Command::ShareFiles(PROJECTS, paths) => !live && paths == [picture.clone()],
                Command::SendLive(PROJECTS, path) => live && path == picture,
                _ => false,
            })
        });
    }
    for icon in [false, true] {
        let next = if icon {
            Ask::Icon(address.clone())
        } else {
            Ask::Picture
        };
        ask(&mut h, next);
        until(&mut h, "the crop dialog", |home| {
            (home.crop.take()).is_some_and(|crop| match crop.target {
                crate::crop::Target::Icon(at) => icon && at == address,
                crate::crop::Target::Avatar => !icon,
                crate::crop::Target::Emoji { .. } => false,
            })
        });
    }
    let home = &mut h.state_mut().as_mut().unwrap().0;
    crate::admin::open(home, 0, crate::admin::Tab::Emoji);
    h.run_steps(2);
    ask(&mut h, Ask::Emoji(address.clone(), false));
    shows(&mut h, &t!("admin.upload_remove", file = "photo.png"));
    *PICKED.lock().unwrap() = vec![zip];
    ask(&mut h, Ask::EmojiZip(address));
    shows(&mut h, t!("admin.import_title"));
}

/// iOS's picker hands over a copy under the file's own name (`files::keep_picked`):
/// it is attached under that name, and removing it deletes the copy, not the original.
#[test]
fn picked_copies_attach_under_their_name_and_go_when_removed() {
    let _picker = PICKER_TEST.lock().unwrap();
    let _reset = ResetPicker;
    let data = tempfile::tempdir().unwrap();
    let original = data.path().join("Holiday photo.jpg");
    std::fs::write(&original, b"synthetic picture").unwrap();
    let kept = files::keep_picked(&original, "Holiday photo.jpg").unwrap();
    assert_eq!(kept.file_name(), original.file_name());
    assert_eq!(std::fs::read(&kept).unwrap(), b"synthetic picture");
    *PICKED.lock().unwrap() = vec![kept.clone()];
    *files::TEST_ANSWER.lock().unwrap() = Some(|name| {
        assert_eq!(name, None);
        PICKED.lock().unwrap().clone()
    });
    let ctx = egui::Context::default();
    let mut picker = files::Files::new(data.path());
    let address: url::Url = "https://example.invalid".parse().unwrap();
    picker.ask(&ctx, Ask::Attach(address, PROJECTS));
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    let mut answers = Vec::new();
    while answers.is_empty() && std::time::Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(5));
        answers = picker.answers();
    }
    let [(Ask::Attach(_, PROJECTS), paths)] = answers.as_slice() else {
        panic!("the pick wasn't answered");
    };
    let mut chat = crate::server::Chat::default();
    assert_eq!(files::stage(&mut chat, paths.clone()), None);
    assert_eq!(chat.staged[0].name, "Holiday photo.jpg");
    files::discard(&chat.staged.remove(0).path);
    assert!(!kept.parent().unwrap().exists());
    assert!(original.exists());
}

/// Staged files are equal square tiles, whatever they are: a phone photo larger than
/// 4096 pixels shows as a picture, other files as an icon with their name. Set
/// `RUSTTALK_STAGED_SCREENSHOTS` to a folder to save the composer.
#[test]
fn staged_files_are_equal_tiles() {
    let _route = crate::route::test_lock();
    let data = tempfile::tempdir().unwrap();
    // A 24 MP phone photo, over the old 4096-pixel decode limit.
    let photo = data.path().join("IMG_2676.jpeg");
    image::RgbImage::from_fn(5712, 4284, |x, y| {
        image::Rgb([(x / 23) as u8, (y / 17) as u8, 160])
    })
    .save(&photo)
    .unwrap();
    let small = data.path().join("Sunset.png");
    image::RgbImage::from_fn(300, 200, |x, _| image::Rgb([250, 120 + (x / 3) as u8, 60]))
        .save(&small)
        .unwrap();
    let notes = data
        .path()
        .join("Trip notes for the weekend in the mountains.pdf");
    std::fs::write(&notes, vec![0; 48_000]).unwrap();
    let sheet = data.path().join("Budget.xlsx");
    std::fs::write(&sheet, vec![0; 9_000]).unwrap();
    let paths = [photo, small, notes, sheet];
    for dark in [true, false] {
        for size in [Vec2::new(390., 844.), Vec2::new(1100., 700.)] {
            let (mut base, _sent) = files_harness(dark);
            let mut home = base.state_mut().take().unwrap();
            home.view = View::Server(0);
            home.servers[0].open = Some(PROJECTS);
            home.layout.list = false;
            home.find.opened = false;
            let mut settings = home.voice.audio.settings();
            settings.language = "en".into();
            settings.set_appearance(ui::Appearance {
                mode: if dark {
                    ui::Mode::Dark
                } else {
                    ui::Mode::Light
                },
                ..Default::default()
            });
            home.voice.audio.set_settings(settings);
            let chat = home.servers[0].chats.get_mut(&PROJECTS).unwrap();
            chat.staged = (paths.iter())
                .map(|path| crate::server::Staged {
                    path: path.clone(),
                    name: path.file_name().unwrap().to_string_lossy().into(),
                    size: std::fs::metadata(path).unwrap().len(),
                })
                .collect();
            let names: Vec<String> = chat.staged.iter().map(|file| file.name.clone()).collect();
            let mut h = Harness::builder()
                .with_context_setup(ui::install)
                .with_size(size)
                .build_ui_state(
                    |ui, (home, updates, keys): &mut Screen| {
                        show(ui, home, updates, keys);
                    },
                    (
                        home,
                        crate::update::Updater::new(data.path()),
                        crate::keystore::without_keychain(data.path()),
                    ),
                );
            let tile = |h: &Harness<'_, Screen>, name: &str| {
                let label = format!("{name} · ");
                h.get_by_label_contains(&label).rect()
            };
            // A decoded picture's tile is a button; it decodes on a thread of its own.
            let decoded = |h: &Harness<'_, Screen>| {
                names[..2].iter().all(|name| {
                    let label = format!("{name} · ");
                    h.query_all_by_role(egui::Role::Button).any(|node| {
                        (node.accesskit_node().label()).is_some_and(|l| l.starts_with(&label))
                    })
                })
            };
            let deadline = std::time::Instant::now() + Duration::from_secs(60);
            h.run_steps(2);
            while !decoded(&h) && std::time::Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(20));
                h.run_steps(1);
            }
            assert!(decoded(&h), "the staged pictures did not decode");
            h.run_steps(2);
            for name in &names {
                assert_eq!(
                    tile(&h, name).size(),
                    Vec2::splat(112.0),
                    "{name} is not a square tile"
                );
            }
            assert_eq!(h.query_all_by_label("Remove").count(), names.len());
            if let Ok(dir) = std::env::var("RUSTTALK_STAGED_SCREENSHOTS") {
                let theme = if dark { "dark" } else { "light" };
                h.render()
                    .unwrap()
                    .save(format!("{dir}/composer-{}-{theme}.png", size.x))
                    .unwrap();
            }
        }
    }
}
