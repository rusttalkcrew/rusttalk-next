use super::*;
use egui_kittest::kittest::Queryable;
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

struct ResetPicker;
impl Drop for ResetPicker {
    fn drop(&mut self) {
        *files::TEST_ANSWER.lock().unwrap() = None;
    }
}

static PICKED: Mutex<Vec<PathBuf>> = Mutex::new(Vec::new());

#[test]
fn oversized_stored_picker_feedback() {
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
            for _ in 0..100 {
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
                    for _ in 0..100 {
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
