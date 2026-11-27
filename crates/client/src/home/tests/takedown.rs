//! Operator takedowns (#679): the owner removes a message with its files from its menu, or
//! by pasted IDs in Server settings > Takedowns, which also lists the records.

use super::*;
use crate::net::{Admin, AdminUpdate, TakeDown, Takedown, TakenDown};
use egui_kittest::{
    Harness,
    kittest::{NodeT, Queryable, by},
};
use rusttalk_core::client::prepare_file;

type Screen = (Home, crate::update::Updater, crate::keystore::Keystore);
type Sent = tokio::sync::mpsc::UnboundedReceiver<Command>;

/// The whole screen as the owner, with a message of Ann's carrying a picture, optionally
/// with server settings open on Takedowns. Returns the picture too.
fn screen(
    size: Vec2,
    dark: bool,
    settings: bool,
) -> (
    Harness<'static, Option<Screen>>,
    Sent,
    rusttalk_core::client::Attachment,
) {
    let (mut files, sent) = files_harness(dark);
    let mut home = files.state_mut().take().unwrap();
    home.view = View::Server(0);
    home.servers[0].open = Some(PROJECTS);
    home.find.opened = false;
    let perms = &mut home.servers[0].dir.perms;
    (perms.owner, perms.ban, perms.admin) = (true, true, true);
    perms.rank = i64::MAX;
    let mut png = std::io::Cursor::new(Vec::new());
    image::RgbaImage::from_pixel(64, 48, image::Rgba([40, 120, 200, 255]))
        .write_to(&mut png, image::ImageFormat::Png)
        .unwrap();
    let png = png.into_inner();
    let (picture, _) = prepare_file("Flyer.png", "image/png", &png, &png).unwrap();
    let message = |id: u8, text: &str, edit_of: Option<Id>, files| Msg {
        id: [id; 32],
        person: ANN,
        at: 1,
        body: Body::Text(text.into()),
        backfilled: false,
        reply_to: None,
        quoted_author: None,
        edit_of,
        files,
        live: None,
        sticker: None,
        replicas: vec![],
        copy_files: vec![],
    };
    let chat = home.servers[0].chats.get_mut(&PROJECTS).unwrap();
    let original = message(7, "Selling this", None, vec![picture.clone()]);
    chat.merge(vec![original], false);
    chat.merge(
        vec![message(8, "Selling these", Some([7; 32]), vec![])],
        false,
    );
    if settings {
        crate::admin::open(&mut home, 0, crate::admin::Tab::Takedowns);
    }
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
    (harness, sent, picture)
}

fn sent_takedowns(sent: &mut Sent) -> Vec<TakeDown> {
    std::iter::from_fn(|| sent.try_recv().ok())
        .filter_map(|command| match command {
            Command::Admin(Admin::TakeDown(takedown)) => Some(takedown),
            _ => None,
        })
        .collect()
}

fn shot(h: &mut Harness<'static, Option<Screen>>, name: &str, size: Vec2, dark: bool) {
    if let Ok(dir) = std::env::var("RUSTTALK_TEST_SCREENSHOTS") {
        let theme = if dark { "dark" } else { "light" };
        h.render()
            .unwrap()
            .save(format!("{dir}/{name}-{}x{}-{theme}.png", size.x, size.y))
            .unwrap();
    }
}

fn type_into(h: &mut Harness<'static, Option<Screen>>, label: &str, text: &str) {
    h.get(by().role(Role::MultilineTextInput).label(label))
        .focus();
    h.run_steps(2);
    h.get(by().role(Role::MultilineTextInput).label(label))
        .type_text(text);
    h.run_steps(4);
}

fn right_click(h: &mut Harness<'static, Option<Screen>>, pos: egui::Pos2) {
    h.event(egui::Event::PointerMoved(pos));
    for pressed in [true, false] {
        h.event(egui::Event::PointerButton {
            pos,
            button: egui::PointerButton::Secondary,
            pressed,
            modifiers: egui::Modifiers::NONE,
        });
        h.run_steps(1);
    }
    h.run_steps(2);
}

#[test]
fn the_owner_removes_a_message_with_its_edits_and_files_from_its_menu() {
    for dark in [true, false] {
        for size in [Vec2::new(390.0, 844.0), Vec2::new(1100.0, 700.0)] {
            let (mut h, mut sent, picture) = screen(size, dark, false);
            if h.query_by_label("Selling these").is_none() {
                // Phones start on the channel list.
                h.state_mut().as_mut().unwrap().0.layout.list = false;
                h.run_steps(6);
            }
            let text = h.get_by_label("Selling these").rect();
            right_click(&mut h, text.center());
            h.get_by_label(t!("takedown.menu")).click();
            h.run_steps(4);
            let remove = by().role(Role::Button).label(t!("takedown.remove_button"));
            assert!(
                h.get(remove).accesskit_node().is_disabled(),
                "needs a reason"
            );
            type_into(
                &mut h,
                t!("takedown.reason"),
                "Counterfeit goods, notice 12",
            );
            h.get_by_label(t!("takedown.ban")).click();
            h.run_steps(4);
            shot(&mut h, "takedown-dialog", size, dark);
            h.get(by().role(Role::Button).label(t!("takedown.remove_button")))
                .click();
            h.run_steps(4);
            let thumbnail = picture.thumbnail.as_ref().unwrap().id;
            assert_eq!(
                sent_takedowns(&mut sent),
                [TakeDown {
                    ids: vec![[7; 32], [8; 32], picture.file.id, thumbnail],
                    reason: "Counterfeit goods, notice 12".into(),
                    notice: String::new(),
                    ban: true,
                }]
            );
        }
    }
    // Only the owner gets it.
    let (mut h, _, _) = screen(Vec2::new(1100.0, 700.0), true, false);
    h.state_mut().as_mut().unwrap().0.servers[0].dir.perms.owner = false;
    h.run_steps(4);
    let text = h.get_by_label("Selling these").rect();
    right_click(&mut h, text.center());
    assert!(h.query_by_label(t!("takedown.menu")).is_none());
}

#[test]
fn takedowns_page_removes_pasted_ids_and_lists_records() {
    for dark in [true, false] {
        for size in [Vec2::new(390.0, 844.0), Vec2::new(1100.0, 700.0)] {
            let (mut h, mut sent, _) = screen(size, dark, true);
            if h.query_by_label(t!("takedown.ids")).is_none() {
                // Phones may start on the page list.
                h.get_by_label(t!("takedown.title")).click();
                h.run_steps(4);
            }
            let record = Takedown {
                at: 1_767_225_600,
                actor: ME,
                reason: "Police notice about a stolen photo".into(),
                notice: Some("Case 2026-0042".into()),
                banned: Some(ANN),
                items: vec![
                    TakenDown {
                        file: false,
                        id: [7; 32],
                        size: 412,
                        sha256: Some("9b".repeat(32)),
                    },
                    TakenDown {
                        file: true,
                        id: [9; 32],
                        size: 20_480,
                        sha256: Some("3f".repeat(32)),
                    },
                ],
            };
            let home = &mut h.state_mut().as_mut().unwrap().0;
            let settings = home.server_settings.as_mut().unwrap();
            settings.apply(AdminUpdate::Takedowns(vec![record]));
            h.run_steps(4);
            assert!(
                h.query_by_label("Police notice about a stolen photo")
                    .is_some()
            );
            type_into(&mut h, t!("takedown.ids"), &hex::encode([7u8; 32]));
            type_into(&mut h, t!("takedown.reason"), "Reported spam");
            shot(&mut h, "takedown-settings", size, dark);
            h.get(by().role(Role::Button).label(t!("takedown.remove_button")))
                .click();
            h.run_steps(4);
            assert_eq!(
                sent_takedowns(&mut sent),
                [TakeDown {
                    ids: vec![[7; 32]],
                    reason: "Reported spam".into(),
                    notice: String::new(),
                    ban: false,
                }]
            );
            // Anything that isn't an ID is pointed out, and nothing can be sent.
            type_into(&mut h, t!("takedown.ids"), " not-an-id");
            assert!(h.query_by_label(t!("takedown.bad_ids")).is_some());
            let remove = by().role(Role::Button).label(t!("takedown.remove_button"));
            assert!(h.get(remove).accesskit_node().is_disabled());
        }
    }
}
