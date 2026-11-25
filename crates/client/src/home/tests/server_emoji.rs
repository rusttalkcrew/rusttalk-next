//! Server settings > Emoji & stickers: deleting one emoji or sticker asks first, and
//! the page fits a phone.

use super::*;
use crate::net::Admin;
use egui_kittest::{Harness, kittest::Queryable};

const PACK: Id = [7; 32];
const OTHER_PACK: Id = [9; 32];
const PARROT: Id = [8; 32];

const SIZES: [(f32, f32); 2] = [(390.0, 844.0), (1100.0, 700.0)];

/// The page with one emoji in "Party"; `full` also adds a second pack and fills
/// every limit, so the upload and add-pack rows show their warnings.
fn harness(
    width: f32,
    height: f32,
    dark: bool,
    full: bool,
) -> (
    Harness<'static, Home>,
    tokio::sync::mpsc::UnboundedReceiver<Command>,
) {
    let (mut fixture, mut sent) = files_harness(dark);
    let mut home = fixture.state_mut().take().unwrap();
    let dir = &mut home.servers[0].dir;
    dir.perms.manage_emoji = true;
    dir.emoji_max = if full { (1, 1) } else { (50, 50) };
    dir.packs = vec![crate::server::EmojiPack {
        id: PACK,
        name: Some("Party".into()),
    }];
    if full {
        dir.packs.push(crate::server::EmojiPack {
            id: OTHER_PACK,
            name: Some("Animals".into()),
        });
    }
    dir.packs_max = if full { 2 } else { 5 };
    dir.emoji = vec![CustomEmoji {
        id: PARROT,
        name: "party_parrot".into(),
        sticker: false,
        pack: PACK,
        picture: None,
    }];
    crate::admin::open(&mut home, 0, crate::admin::Tab::Emoji);
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
            |ui, home: &mut Home| crate::admin::window(ui.ctx(), home),
            home,
        );
    h.run_steps(4);
    if ui::is_phone(&h.ctx) {
        // Phones list the pages first.
        h.get_by_label(t!("admin.emoji_stickers")).click();
        h.run_steps(4);
    }
    while sent.try_recv().is_ok() {}
    (h, sent)
}

fn deletes(sent: &mut tokio::sync::mpsc::UnboundedReceiver<Command>) -> usize {
    std::iter::from_fn(|| sent.try_recv().ok())
        .filter(|cmd| matches!(cmd, Command::Admin(Admin::DeleteEmoji(PARROT))))
        .count()
}

/// The emoji's own actions: under "…" on a phone, beside it on wider windows.
fn more_label() -> String {
    t!("admin.member_actions", name = ":party_parrot:")
}

/// Press the emoji's own Delete.
fn ask_delete(h: &mut Harness<'static, Home>) {
    if ui::is_phone(&h.ctx) {
        h.get_by_label(&more_label()).click();
        h.run();
    }
    h.query_all_by_label(t!("common.delete"))
        .last()
        .unwrap()
        .click_accesskit();
    h.run();
}

#[test]
fn deleting_an_emoji_asks_first() {
    let question = t!("admin.emoji_delete_help", name = "party_parrot");
    for ((width, height), dark) in SIZES
        .into_iter()
        .flat_map(|size| [(size, true), (size, false)])
    {
        let (mut h, mut sent) = harness(width, height, dark, false);
        let phone = ui::is_phone(&h.ctx);
        assert!(h.query_by_label(&question).is_none());

        // The pack's Delete comes first, then the emoji's (in its "…" menu on phones).
        let shown = if phone { 1 } else { 2 };
        assert_eq!(h.query_all_by_label(t!("common.delete")).count(), shown);

        // Delete only asks; Cancel takes the question away and sends nothing.
        ask_delete(&mut h);
        assert!(h.query_by_label(&question).is_some());
        assert_eq!(deletes(&mut sent), 0);
        h.get_by_label(t!("common.cancel")).click();
        h.run_steps(2);
        assert!(h.query_by_label(&question).is_none());
        assert_eq!(deletes(&mut sent), 0);

        // Asked again, Delete in the question sends it once.
        ask_delete(&mut h);
        assert_eq!(deletes(&mut sent), 0);
        if let Some(dir) = std::env::var_os("RUSTTALK_EMOJI_DELETE_SCREENSHOTS") {
            let theme = if dark { "dark" } else { "light" };
            h.render()
                .unwrap()
                .save(PathBuf::from(dir).join(format!("{theme}-{width}x{height}.png")))
                .unwrap();
        }
        let window = h.ctx.content_rect();
        let card = h.get_by_label(&question).rect();
        assert!(card.max.x <= window.max.x, "the question fits: {card:?}");
        let buttons: Vec<_> = h.query_all_by_label(t!("common.delete")).collect();
        assert_eq!(
            buttons.len(),
            shown + 1,
            "the pack's, the row's and the question's Delete"
        );
        let confirm = buttons.last().unwrap();
        assert!(confirm.rect().max.x <= window.max.x);
        confirm.click();
        h.run_steps(2);
        assert_eq!(deletes(&mut sent), 1);
    }
}

#[test]
fn the_page_fits_the_window() {
    for ((width, height), dark) in SIZES
        .into_iter()
        .flat_map(|size| [(size, true), (size, false)])
    {
        let (mut h, mut sent) = harness(width, height, dark, true);
        let phone = ui::is_phone(&h.ctx);
        if let Some(dir) = std::env::var_os("RUSTTALK_EMOJI_PAGE_SCREENSHOTS") {
            let theme = if dark { "dark" } else { "light" };
            h.render()
                .unwrap()
                .save(PathBuf::from(dir).join(format!("{theme}-{width}x{height}.png")))
                .unwrap();
        }
        let window = h.ctx.content_rect();
        // Within the window's width; long pages scroll down.
        let inside = |h: &Harness<'static, Home>, label: &str| {
            let rects: Vec<_> = h.query_all_by_label(label).map(|n| n.rect()).collect();
            assert!(!rects.is_empty(), "{label} at {width}x{height} is missing");
            for rect in rects {
                assert!(
                    window.x_range().contains(rect.min.x) && window.x_range().contains(rect.max.x),
                    "{label} at {width}x{height}: {rect:?} outside {window:?}"
                );
            }
        };
        for label in [
            t!("admin.pick_picture"),
            t!("admin.emoji_full"),
            t!("admin.pack_add"),
            t!("admin.pack_limit"),
        ] {
            inside(&h, label);
        }
        if phone {
            // Rename, Move to and Delete are under "…", and the menu fits too.
            assert!(h.query_by_label(t!("admin.move_to")).is_none());
            inside(&h, &more_label());
            h.get_by_label(&more_label()).click();
            h.run();
            for label in [t!("common.rename"), "Animals"] {
                inside(&h, label);
            }
            // The menu's Delete comes after the pack's.
            inside(&h, t!("common.delete"));
            if let Some(dir) = std::env::var_os("RUSTTALK_EMOJI_PAGE_SCREENSHOTS") {
                let theme = if dark { "dark" } else { "light" };
                h.render()
                    .unwrap()
                    .save(PathBuf::from(dir).join(format!("{theme}-{width}x{height}-menu.png")))
                    .unwrap();
            }
            // The menu opens under the "Animals" pack's title.
            h.query_all_by_label("Animals")
                .max_by(|a, b| a.rect().min.y.total_cmp(&b.rect().min.y))
                .unwrap()
                .click();
            h.run();
            assert!(
                std::iter::from_fn(|| sent.try_recv().ok())
                    .any(|cmd| matches!(cmd, Command::Admin(Admin::MoveEmoji(PARROT, OTHER_PACK))))
            );
        } else {
            // Wider windows keep the buttons beside the emoji.
            assert!(h.query_by_label(&more_label()).is_none());
            for label in [t!("common.rename"), t!("admin.move_to")] {
                inside(&h, label);
            }
            // Both packs' Delete and the emoji's.
            assert_eq!(h.query_all_by_label(t!("common.delete")).count(), 3);
        }
    }
}

/// The typed name stays until the upload went through, so a cancelled pick or a
/// refused upload doesn't lose it (#533).
#[test]
fn emoji_name_stays_until_the_upload_went_through() {
    use crate::net::AdminUpdate;
    use std::sync::atomic::{AtomicBool, Ordering};
    static ASKED: AtomicBool = AtomicBool::new(false);
    let _picker = super::uploads::PICKER_TEST
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let _reset = super::uploads::ResetPicker;
    ASKED.store(false, Ordering::SeqCst);
    // The picker is cancelled: no file comes back.
    *files::TEST_ANSWER.lock().unwrap() = Some(|_| {
        ASKED.store(true, Ordering::SeqCst);
        Vec::new()
    });
    let (mut h, _sent) = harness(1100.0, 700.0, true, false);
    let name = |h: &Harness<'static, Home>| {
        h.state()
            .server_settings
            .as_ref()
            .unwrap()
            .emoji_name
            .clone()
    };
    let apply = |h: &mut Harness<'static, Home>, update| {
        h.state_mut()
            .server_settings
            .as_mut()
            .unwrap()
            .apply(update);
        h.run();
    };
    h.state_mut().server_settings.as_mut().unwrap().emoji_name = "cat_wave".into();
    h.run();
    h.get_by_label(t!("admin.pick_picture")).click();
    h.run();
    let start = std::time::Instant::now();
    while !ASKED.load(Ordering::SeqCst) {
        assert!(
            start.elapsed().as_secs() < 10,
            "Pick picture asked for no file"
        );
        std::thread::yield_now();
    }
    h.run();
    assert_eq!(name(&h), "cat_wave", "cancelling the pick keeps the name");

    apply(&mut h, AdminUpdate::Failed("too large".into()));
    assert_eq!(name(&h), "cat_wave", "a refused upload keeps the name");
    // Another change finishing, or another emoji added, leaves it alone too.
    apply(&mut h, AdminUpdate::EmojiAdded("party_cat".into()));
    apply(&mut h, AdminUpdate::Done("done"));
    assert_eq!(name(&h), "cat_wave");

    apply(&mut h, AdminUpdate::EmojiAdded("cat_wave".into()));
    apply(&mut h, AdminUpdate::Done("done"));
    assert_eq!(name(&h), "", "the name clears once it was added");
}
