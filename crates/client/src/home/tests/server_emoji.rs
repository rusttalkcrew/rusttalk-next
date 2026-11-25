//! Server settings > Emoji and > Stickers: separate pages, picked pictures checked
//! before Upload, the editor and the pack menu, on a phone and a desktop.

use super::*;
use crate::net::{Admin, AdminUpdate};
use egui_kittest::{
    Harness,
    kittest::{NodeT, Queryable, by},
};

const PACK: Id = [7; 32];
const OTHER_PACK: Id = [9; 32];
const PARROT: Id = [8; 32];
const HUG: Id = [6; 32];

const PHONE: (f32, f32) = (390.0, 844.0);
const DESKTOP: (f32, f32) = (1100.0, 700.0);

/// Where screenshots go, when asked for.
fn shots() -> Option<PathBuf> {
    std::env::var_os("RUSTTALK_EMOJI_SCREENSHOTS").map(PathBuf::from)
}

fn shoot(h: &mut Harness<'static, Home>, name: &str, dark: bool) {
    if let Some(dir) = shots() {
        let theme = if dark { "dark" } else { "light" };
        let size = h.ctx.content_rect().size();
        h.render()
            .unwrap()
            .save(dir.join(format!("{name}-{theme}-{}x{}.png", size.x, size.y)))
            .unwrap();
    }
}

fn picture(key: u8, side: u32, rgba: [u8; 4]) -> Picture {
    let mut bytes = std::io::Cursor::new(Vec::new());
    image::RgbaImage::from_pixel(side, side, image::Rgba(rgba))
        .write_to(&mut bytes, image::ImageFormat::Png)
        .unwrap();
    let pixels = [rgba; 1].concat().repeat((side * side) as usize);
    let image = ColorImage::from_rgba_unmultiplied([side as usize; 2], &pixels);
    Picture::new([key; 32], vec![(image, 0)], Arc::new(bytes.into_inner()))
}

/// Server settings on the Emoji page (or `sticker`, Stickers) with ":party_parrot:"
/// and the sticker ":big_hug:" in "Party", and an empty "Animals" pack.
fn harness(
    (width, height): (f32, f32),
    dark: bool,
    sticker: bool,
) -> (
    Harness<'static, Home>,
    tokio::sync::mpsc::UnboundedReceiver<Command>,
) {
    let (mut fixture, mut sent) = files_harness(dark);
    let mut home = fixture.state_mut().take().unwrap();
    let dir = &mut home.servers[0].dir;
    dir.perms.manage_emoji = true;
    dir.emoji_max = (50, 20);
    dir.packs = vec![
        crate::server::EmojiPack {
            id: PACK,
            name: Some("Party".into()),
        },
        crate::server::EmojiPack {
            id: OTHER_PACK,
            name: Some("Animals".into()),
        },
    ];
    dir.packs_max = 5;
    dir.emoji = vec![
        CustomEmoji {
            id: PARROT,
            name: "party_parrot".into(),
            sticker: false,
            pack: PACK,
            picture: Some(picture(1, 48, [40, 180, 90, 255])),
        },
        CustomEmoji {
            id: HUG,
            name: "big_hug".into(),
            sticker: true,
            pack: PACK,
            picture: Some(picture(2, 120, [220, 120, 40, 255])),
        },
    ];
    let tab = if sticker {
        crate::admin::Tab::Stickers
    } else {
        crate::admin::Tab::Emoji
    };
    crate::admin::open(&mut home, 0, tab);
    let theme = if dark {
        egui::Theme::Dark
    } else {
        egui::Theme::Light
    };
    let mut h = Harness::builder()
        .with_size(Vec2::new(width, height))
        .with_context_setup(ui::install)
        .with_theme(theme)
        .build_ui_state(
            |ui, home: &mut Home| {
                crate::admin::take_drops(ui.ctx(), home);
                crate::admin::window(ui.ctx(), home);
            },
            home,
        );
    h.run_steps(4);
    if ui::is_phone(&h.ctx) {
        // Phones list the pages first.
        let page = if sticker {
            t!("admin.stickers")
        } else {
            t!("admin.emoji")
        };
        h.get_by_label(page).click();
        h.run_steps(4);
    }
    while sent.try_recv().is_ok() {}
    (h, sent)
}

fn sent_now(sent: &mut tokio::sync::mpsc::UnboundedReceiver<Command>) -> Vec<Admin> {
    std::iter::from_fn(|| sent.try_recv().ok())
        .filter_map(|cmd| match cmd {
            Command::Admin(admin) => Some(admin),
            _ => None,
        })
        .collect()
}

fn edit_label(name: &str) -> String {
    t!("admin.edit_item", name = format!(":{name}:"))
}

fn enabled(h: &Harness<'static, Home>, label: &str) -> bool {
    !h.get(by().role(egui::accesskit::Role::Button).label(label))
        .accesskit_node()
        .is_disabled()
}

#[test]
fn emoji_and_stickers_have_their_own_pages() {
    for size in [DESKTOP, PHONE] {
        for dark in [true, false] {
            let (mut h, _sent) = harness(size, dark, false);
            assert!(h.query_by_label(&edit_label("party_parrot")).is_some());
            assert!(h.query_by_label(&edit_label("big_hug")).is_none());
            h.get_by_label(&t!("admin.emoji_used", count = 1, max = 50));
            shoot(&mut h, "emoji", dark);

            let (mut h, _sent) = harness(size, dark, true);
            assert!(h.query_by_label(&edit_label("big_hug")).is_some());
            assert!(h.query_by_label(&edit_label("party_parrot")).is_none());
            h.get_by_label(&t!("admin.stickers_used", count = 1, max = 20));
            // Every tile and button fits the window.
            let window = h.ctx.content_rect();
            for label in [edit_label("big_hug"), t!("admin.pack_new").into()] {
                let rect = h.get_by_label(&label).rect();
                assert!(window.x_range().contains(rect.max.x), "{label}: {rect:?}");
            }
            shoot(&mut h, "stickers", dark);
        }
    }
    // Desktop: the sidebar switches between the two.
    let (mut h, _sent) = harness(DESKTOP, true, false);
    h.query_all_by_label(t!("admin.stickers"))
        .next()
        .unwrap()
        .click();
    h.run_steps(4);
    assert!(h.query_by_label(&edit_label("big_hug")).is_some());
    assert!(h.query_by_label(&edit_label("party_parrot")).is_none());
}

#[derive(Debug)]
struct DropPath(PathBuf);

impl egui::DroppedFile for DropPath {
    fn path(&self) -> &std::path::Path {
        &self.0
    }
    fn bytes(&self) -> Result<Vec<u8>, String> {
        Err("read from the path".into())
    }
}

/// Run frames until no picked picture is still being made ready.
fn settle(h: &mut Harness<'static, Home>) {
    let start = std::time::Instant::now();
    loop {
        h.run_steps(2);
        if h.query_all_by_label(t!("admin.upload_preparing"))
            .next()
            .is_none()
        {
            return;
        }
        assert!(start.elapsed().as_secs() < 20, "pictures never got ready");
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
}

#[test]
fn several_pictures_are_checked_before_one_upload() {
    let files = tempfile::tempdir().unwrap();
    let png = |name: &str, rgba: [u8; 4]| {
        let path = files.path().join(name);
        image::RgbaImage::from_pixel(200, 160, image::Rgba(rgba))
            .save(&path)
            .unwrap();
        path
    };
    let cat = png("party_cat.png", [200, 60, 60, 255]);
    let wave = png("Wave-Hello.png", [60, 60, 200, 255]);
    let taken = png("party parrot.png", [60, 200, 60, 255]);
    let text = files.path().join("notes.png");
    std::fs::write(&text, "not a picture").unwrap();
    let huge = files.path().join("huge.gif");
    std::fs::File::create(&huge)
        .unwrap()
        .set_len(9 * 1024 * 1024)
        .unwrap();
    for size in [DESKTOP, PHONE] {
        for dark in [true, false] {
            let (mut h, mut sent) = harness(size, dark, false);
            h.input_mut().dropped_files = [&cat, &wave, &taken, &text, &huge]
                .into_iter()
                .map(|path| Arc::new(DropPath(path.clone())) as egui::DroppedFileHandle)
                .collect();
            settle(&mut h);
            // Names come from the file names.
            let name = |h: &Harness<'static, Home>, file: &str| {
                h.get(
                    by().role(egui::accesskit::Role::TextInput)
                        .label(&t!("admin.upload_name", file = file)),
                )
                .value()
            };
            assert_eq!(name(&h, "Wave-Hello.png").as_deref(), Some("wave_hello"));
            assert_eq!(
                name(&h, "party parrot.png").as_deref(),
                Some("party_parrot")
            );
            // What is wrong shows beside each picture, and Upload waits.
            for problem in [
                t!("admin.name_taken"),
                t!("admin.upload_wrong_type"),
                t!("admin.upload_too_big"),
            ] {
                h.get_by_label(problem);
            }
            h.get_by_label(&t!("admin.upload_fix", count = 3));
            let upload = t!("admin.upload_emoji", count = 5);
            assert!(!enabled(&h, &upload));
            shoot(&mut h, "upload-checks", dark);

            // A new name fixes the taken one; the others are removed.
            h.get(
                by().role(egui::accesskit::Role::TextInput)
                    .label(&t!("admin.upload_name", file = "party parrot.png")),
            )
            .focus();
            h.run_steps(2);
            h.get(
                by().role(egui::accesskit::Role::TextInput)
                    .label(&t!("admin.upload_name", file = "party parrot.png")),
            )
            .type_text("_2");
            h.run_steps(2);
            for file in ["notes.png", "huge.gif"] {
                h.get_by_label(&t!("admin.upload_remove", file = file))
                    .click();
                h.run_steps(2);
            }
            assert!(h.query_by_label(t!("admin.name_taken")).is_none());
            let upload = t!("admin.upload_emoji", count = 3);
            assert!(enabled(&h, &upload));
            shoot(&mut h, "upload-ready", dark);
            h.get_by_label(&upload).click();
            h.run_steps(2);
            let sent = sent_now(&mut sent);
            let [Admin::Batch(adds)] = sent.as_slice() else {
                panic!("one batch of uploads: {}", sent.len());
            };
            let names: Vec<_> = adds
                .iter()
                .map(|add| match add {
                    Admin::AddEmoji {
                        name,
                        sticker: false,
                        pack: Some(PACK),
                        bytes,
                    } if !bytes.is_empty() => name.as_str(),
                    _ => panic!("not an emoji upload to the first pack"),
                })
                .collect();
            assert_eq!(names, ["party_cat", "wave_hello", "party_parrot_2"]);

            // What went through leaves the list; what was refused stays to try again.
            let settings = h.state_mut().server_settings.as_mut().unwrap();
            settings.apply(AdminUpdate::EmojiAdded("party_cat".into()));
            settings.apply(AdminUpdate::EmojiAdded("wave_hello".into()));
            settings.apply(AdminUpdate::PartlySaved {
                failed: vec![2],
                text: "busy".into(),
            });
            h.run_steps(2);
            assert!(
                h.query_by_label(&t!("admin.upload_name", file = "party_cat.png"))
                    .is_none()
            );
            assert!(enabled(&h, &t!("admin.upload_emoji", count = 1)));
        }
    }
}

#[test]
fn the_editor_renames_moves_and_deletes() {
    for size in [DESKTOP, PHONE] {
        for dark in [true, false] {
            let (mut h, mut sent) = harness(size, dark, false);
            h.get_by_label(&edit_label("party_parrot")).click();
            h.run_steps(4);
            h.get_by_label(t!("admin.edit_emoji"));
            assert!(!enabled(&h, t!("common.save")), "nothing changed yet");
            shoot(&mut h, "editor", dark);

            // A new name and another pack are saved together.
            let field = by()
                .role(egui::accesskit::Role::TextInput)
                .label(t!("common.name"));
            h.get(field.clone()).focus();
            h.run_steps(2);
            h.get(field).type_text("_2");
            h.run_steps(2);
            h.get(by().role(egui::accesskit::Role::ComboBox)).click();
            h.run_steps(2);
            h.query_all_by_label("Animals").last().unwrap().click();
            h.run_steps(2);
            h.get_by_label(t!("common.save")).click();
            h.run_steps(2);
            let sent_list = sent_now(&mut sent);
            let [Admin::Batch(changes)] = sent_list.as_slice() else {
                panic!("rename and move together");
            };
            assert!(matches!(
                changes.as_slice(),
                [Admin::RenameEmoji(PARROT, name), Admin::MoveEmoji(PARROT, OTHER_PACK)]
                    if name == "party_parrot_2"
            ));
            // Done closes it.
            let settings = h.state_mut().server_settings.as_mut().unwrap();
            settings.apply(AdminUpdate::Done("Renamed."));
            h.run_steps(4);
            assert!(h.query_by_label(t!("admin.edit_emoji")).is_none());

            // Delete asks first; Cancel goes back to the editor.
            h.get_by_label(&edit_label("party_parrot")).click();
            h.run_steps(4);
            // The dialog changes size: let it settle where it is drawn before each click.
            h.get_by_label(t!("common.delete")).click();
            h.run_steps(6);
            let question = t!("admin.emoji_delete_help", name = "party_parrot");
            h.get_by_label(&question);
            assert!(sent_now(&mut sent).is_empty());
            shoot(&mut h, "editor-delete", dark);
            h.get_by_label(t!("common.cancel")).click();
            h.run_steps(6);
            assert!(h.query_by_label(&question).is_none());
            h.get_by_label(t!("admin.edit_emoji"));
            h.get_by_label(t!("common.delete")).click();
            h.run_steps(6);
            h.get_by_label(t!("common.delete")).click();
            h.run_steps(2);
            assert!(matches!(
                sent_now(&mut sent).as_slice(),
                [Admin::DeleteEmoji(PARROT)]
            ));
        }
    }
}

/// Open the "…" menu of a pack and pick `item`.
fn pack_menu(h: &mut Harness<'static, Home>, pack: &str, item: &str) {
    h.get_by_label(&t!("admin.pack_actions", name = pack))
        .click();
    h.run_steps(2);
    h.get_by_label(item).click();
    h.run_steps(2);
}

#[test]
fn packs_fold_and_their_menu_renames_moves_and_deletes() {
    for size in [DESKTOP, PHONE] {
        for dark in [true, false] {
            let (mut h, mut sent) = harness(size, dark, false);
            // Folding hides the pack's emoji.
            let fold = t!("admin.pack_fold", name = "Party", count = 1);
            h.get_by_label(&fold).click();
            h.run_steps(4);
            assert!(h.query_by_label(&edit_label("party_parrot")).is_none());
            h.get_by_label(&fold).click();
            h.run_steps(4);
            assert!(h.query_by_label(&edit_label("party_parrot")).is_some());

            h.get_by_label(&t!("admin.pack_actions", name = "Party"))
                .click();
            h.run_steps(2);
            // The first pack can't move up.
            assert!(
                h.get_by_label(t!("admin.move_up"))
                    .accesskit_node()
                    .is_disabled()
            );
            shoot(&mut h, "pack-menu", dark);
            h.get_by_label(t!("admin.move_down")).click();
            h.run_steps(2);
            assert!(matches!(
                sent_now(&mut sent).as_slice(),
                [Admin::OrderPacks(ids)] if ids == &[OTHER_PACK, PACK]
            ));

            pack_menu(&mut h, "Party", t!("admin.pack_rename"));
            let field = by()
                .role(egui::accesskit::Role::TextInput)
                .label(t!("admin.pack_name"));
            h.get(field.clone()).type_text(" time");
            h.run_steps(2);
            h.get_by_label(t!("common.save")).click();
            h.run_steps(2);
            assert!(matches!(
                sent_now(&mut sent).as_slice(),
                [Admin::RenamePack(PACK, name)] if name == "Party time"
            ));
            h.state_mut()
                .server_settings
                .as_mut()
                .unwrap()
                .apply(AdminUpdate::Done("Renamed."));
            h.run_steps(4);

            // Deleting a pack asks first and says what goes with it.
            pack_menu(&mut h, "Party", t!("admin.pack_delete"));
            h.get_by_label(&t!("admin.pack_delete_what", count = 2));
            assert!(sent_now(&mut sent).is_empty());
            shoot(&mut h, "pack-delete", dark);
            h.query_all_by_label(t!("admin.pack_delete"))
                .last()
                .unwrap()
                .click();
            h.run_steps(2);
            assert!(matches!(
                sent_now(&mut sent).as_slice(),
                [Admin::DeletePack(PACK)]
            ));
            h.state_mut()
                .server_settings
                .as_mut()
                .unwrap()
                .apply(AdminUpdate::Done("Deleted."));
            h.run_steps(4);

            // New pack.
            h.get_by_label(t!("admin.pack_new")).click();
            h.run_steps(2);
            h.get(field).type_text("Cats");
            h.run_steps(2);
            h.get_by_label(t!("admin.pack_create")).click();
            h.run_steps(2);
            assert!(matches!(
                sent_now(&mut sent).as_slice(),
                [Admin::AddPack(name)] if name == "Cats"
            ));
        }
    }
}
