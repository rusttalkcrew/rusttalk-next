//! Server settings > Emoji & stickers: deleting one emoji or sticker asks first.

use super::*;
use crate::net::Admin;
use egui_kittest::{Harness, kittest::Queryable};

const PACK: Id = [7; 32];
const PARROT: Id = [8; 32];

fn harness(
    width: f32,
    height: f32,
    dark: bool,
) -> (
    Harness<'static, Home>,
    tokio::sync::mpsc::UnboundedReceiver<Command>,
) {
    let (mut fixture, mut sent) = files_harness(dark);
    let mut home = fixture.state_mut().take().unwrap();
    let dir = &mut home.servers[0].dir;
    dir.perms.manage_emoji = true;
    dir.emoji_max = (50, 50);
    dir.packs_max = 5;
    dir.packs = vec![crate::server::EmojiPack {
        id: PACK,
        name: Some("Party".into()),
    }];
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

#[test]
fn deleting_an_emoji_asks_first() {
    let question = t!("admin.emoji_delete_help", name = "party_parrot");
    for ((width, height), dark) in [(390.0, 844.0), (1100.0, 700.0)]
        .into_iter()
        .flat_map(|size| [(size, true), (size, false)])
    {
        let (mut h, mut sent) = harness(width, height, dark);
        assert!(h.query_by_label(&question).is_none());

        // The pack's Delete comes first, then the emoji's.
        assert_eq!(h.query_all_by_label(t!("common.delete")).count(), 2);

        // Delete only asks; Cancel takes the question away and sends nothing.
        h.query_all_by_label(t!("common.delete"))
            .last()
            .unwrap()
            .click_accesskit();
        h.run();
        assert!(h.query_by_label(&question).is_some());
        assert_eq!(deletes(&mut sent), 0);
        h.get_by_label(t!("common.cancel")).click();
        h.run_steps(2);
        assert!(h.query_by_label(&question).is_none());
        assert_eq!(deletes(&mut sent), 0);

        // Asked again, Delete in the question sends it once.
        h.query_all_by_label(t!("common.delete"))
            .last()
            .unwrap()
            .click_accesskit();
        h.run();
        assert_eq!(deletes(&mut sent), 0);
        if let Some(dir) = std::env::var_os("RUSTTALK_EMOJI_DELETE_SCREENSHOTS") {
            let theme = if dark { "dark" } else { "light" };
            h.render()
                .unwrap()
                .save(PathBuf::from(dir).join(format!("{theme}-{width}x{height}.png")))
                .unwrap();
        }
        let buttons: Vec<_> = h.query_all_by_label(t!("common.delete")).collect();
        assert_eq!(
            buttons.len(),
            3,
            "the pack's, the row's and the question's Delete"
        );
        buttons[2].click();
        h.run_steps(2);
        assert_eq!(deletes(&mut sent), 1);
    }
}
