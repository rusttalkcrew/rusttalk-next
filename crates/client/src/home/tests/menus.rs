//! The right-click menus of messages and of the pictures in them.
use super::*;
use crate::net::Fetch;
use egui_kittest::{Harness, kittest::Queryable};
use rusttalk_core::client::{Attachment, decode_frames, prepare_file};

fn msg(id: u8, person: Id, text: &str, files: Vec<Attachment>) -> Msg {
    Msg {
        id: [id; 32],
        person,
        at: 1,
        body: Body::Text(text.into()),
        backfilled: false,
        reply_to: None,
        quoted_author: None,
        edit_of: None,
        files,
        live: None,
        sticker: None,
        replicas: vec![],
        copy_files: vec![],
    }
}

fn right_click(h: &mut Harness<'_, Option<Home>>, pos: egui::Pos2) {
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

fn close_menu(h: &mut Harness<'_, Option<Home>>) {
    h.key_press(egui::Key::Escape);
    h.run_steps(2);
}

/// Also counts the hover toolbar's buttons, which share some labels.
fn shows(h: &Harness<'_, Option<Home>>, label: &str) -> bool {
    h.query_all_by_label(label).next().is_some()
}

/// Where a texture is painted.
fn painted(h: &Harness<'_, Option<Home>>, texture: egui::TextureId) -> egui::Pos2 {
    h.ctx
        .tessellate(h.output().shapes.clone(), h.output().pixels_per_point)
        .iter()
        .find_map(|s| match &s.primitive {
            egui::epaint::Primitive::Mesh(mesh) if mesh.texture_id == texture => {
                Some(mesh.calc_bounds().center())
            }
            _ => None,
        })
        .expect("picture is painted")
}

/// A chat with a text message of mine, one of Ann's, and a photo of Ann's with
/// its thumbnail loaded. Returns the photo, its texture and the sent commands.
fn chat(
    dark: bool,
    size: Vec2,
) -> (
    Harness<'static, Option<Home>>,
    Attachment,
    egui::TextureId,
    tokio::sync::mpsc::UnboundedReceiver<Command>,
) {
    let (mut h, mut sent) = files_harness(dark);
    h.set_size(size);
    h.ctx.data_mut(|d| d.remove::<bool>(files_id(PROJECTS)));
    let photo = image::RgbaImage::from_fn(640, 360, |x, y| {
        image::Rgba([(x / 3) as u8, (y * 2 / 3) as u8, 160, 255])
    });
    let mut png = std::io::Cursor::new(Vec::new());
    photo.write_to(&mut png, image::ImageFormat::Png).unwrap();
    let png = png.into_inner();
    let (file, thumbnail) = prepare_file("Beach.png", "image/png", &png, &png).unwrap();
    let home = h.state_mut().as_mut().unwrap();
    let address = home.servers[0].address.clone();
    let messages = &mut home.servers[0].chats.get_mut(&PROJECTS).unwrap().messages;
    messages.push(msg(1, ME, "See you tomorrow", vec![]));
    messages.push(msg(2, ANN, "Sounds good", vec![]));
    messages.push(msg(3, ANN, "Look at this", vec![file.clone()]));
    h.run_steps(4);
    while sent.try_recv().is_ok() {}
    let frames = decode_frames(&thumbnail.unwrap(), None, 64 * 1024 * 1024).unwrap();
    let ctx = h.ctx.clone();
    h.state_mut().as_mut().unwrap().apply(
        &ctx,
        Update::Image(
            address,
            file.thumbnail.clone().unwrap(),
            Fetch::Thumbnail,
            Ok(frames.into_iter().map(crate::net::frame_image).collect()),
        ),
        0.0,
    );
    let texture = *ctx
        .tex_manager()
        .read()
        .allocated()
        .max_by_key(|(id, _)| **id)
        .unwrap()
        .0;
    h.run_steps(4);
    (h, file, texture, sent)
}

fn screenshot(h: &mut Harness<'_, Option<Home>>, name: &str) {
    if let Ok(dir) = std::env::var("RUSTTALK_TEST_SCREENSHOTS") {
        std::fs::create_dir_all(&dir).unwrap();
        h.render()
            .unwrap()
            .save(format!("{dir}/{name}.png"))
            .unwrap();
    }
}

#[test]
fn message_menus_offer_only_allowed_actions_in_order() {
    let (mut h, _, _, _sent) = chat(true, Vec2::new(720.0, 640.0));
    let mine = h.get_by_label("See you tomorrow").rect().center() + egui::vec2(250.0, 0.0);
    right_click(&mut h, mine);
    for label in [
        t!("common.reply"),
        t!("common.edit"),
        t!("home.copy_text"),
        t!("home.copy_message_id"),
        t!("common.delete"),
    ] {
        assert!(shows(&h, label), "{label}");
    }
    assert!(!shows(&h, t!("files.copy_image")));
    // Delete comes last, below Copy message ID.
    let id = h.get_by_label(t!("home.copy_message_id")).rect().top();
    let delete = (h.query_all_by_label(t!("common.delete")))
        .map(|node| node.rect().top())
        .fold(f32::MIN, f32::max);
    assert!(delete > id);
    h.get_by_label(t!("home.copy_message_id")).click();
    h.step();
    let copied = hex::encode([1u8; 32]);
    assert!(
        h.output()
            .platform_output
            .commands
            .contains(&egui::OutputCommand::CopyText(copied))
    );
    h.run_steps(2);

    let theirs = h.get_by_label("Sounds good").rect().center() + egui::vec2(250.0, 0.0);
    right_click(&mut h, theirs);
    assert!(shows(&h, t!("common.reply")));
    assert!(shows(&h, t!("home.copy_message_id")));
    assert!(!shows(&h, t!("common.edit")), "not mine to edit");
    assert!(!shows(&h, t!("common.delete")), "no right to delete");
    close_menu(&mut h);
}

#[test]
fn picture_menu_copies_saves_and_opens_above_the_message_actions() {
    let (mut h, file, texture, mut sent) = chat(true, Vec2::new(720.0, 640.0));
    let pos = painted(&h, texture);
    right_click(&mut h, pos);
    for label in [
        t!("files.copy_image"),
        t!("files.save_as"),
        t!("files.open_viewer"),
        t!("files.open_default"),
        t!("common.reply"),
        t!("home.copy_message_id"),
    ] {
        assert!(shows(&h, label), "{label}");
    }
    assert!(!shows(&h, t!("common.edit")));
    assert!(!shows(&h, t!("common.delete")));
    assert!(!shows(&h, t!("files.show_in_folder")), "not saved here");
    let copy = h.get_by_label(t!("files.copy_image")).rect().top();
    assert!(h.get_by_label(t!("common.reply")).rect().top() > copy);

    h.get_by_label(t!("files.copy_image")).click();
    h.run_steps(2);
    let asked: Vec<_> = std::iter::from_fn(|| sent.try_recv().ok())
        .filter_map(|command| match command {
            Command::Image(blob, fetch) => Some((blob.id, fetch)),
            _ => None,
        })
        .collect();
    assert_eq!(asked, [(file.file.id, Fetch::Copy)]);
    let image = egui::ColorImage::filled([4, 4], egui::Color32::RED);
    let ctx = h.ctx.clone();
    let address = h.state().as_ref().unwrap().servers[0].address.clone();
    h.state_mut().as_mut().unwrap().apply(
        &ctx,
        Update::Image(
            address,
            file.file.clone(),
            Fetch::Copy,
            Ok(vec![(image, 0)]),
        ),
        0.0,
    );
    h.step();
    assert!(
        (h.output().platform_output.commands.iter())
            .any(|command| matches!(command, egui::OutputCommand::CopyImage(_)))
    );

    let pos = painted(&h, texture);
    right_click(&mut h, pos);
    h.get_by_label(t!("files.open_viewer")).click();
    h.run_steps(2);
    let asked: Vec<_> = std::iter::from_fn(|| sent.try_recv().ok())
        .filter_map(|command| match command {
            Command::Image(blob, fetch) => Some((blob.id, fetch)),
            _ => None,
        })
        .collect();
    assert_eq!(asked, [(file.file.id, Fetch::Full)]);
}

#[test]
fn menu_screenshots() {
    for (width, height) in [(1100.0, 700.0), (390.0, 844.0)] {
        for dark in [true, false] {
            let theme = if dark { "dark" } else { "light" };
            let (mut h, _, texture, _sent) = chat(dark, Vec2::new(width, height));
            let pos = painted(&h, texture);
            right_click(&mut h, pos);
            assert!(shows(&h, t!("files.copy_image")));
            screenshot(&mut h, &format!("picture-menu-{theme}-{width}x{height}"));
            close_menu(&mut h);
            let mine = h.get_by_label("See you tomorrow").rect().center() + egui::vec2(250.0, 0.0);
            right_click(&mut h, mine);
            assert!(shows(&h, t!("common.delete")));
            screenshot(&mut h, &format!("message-menu-{theme}-{width}x{height}"));
        }
    }
}

#[test]
fn holding_a_picture_on_a_phone_opens_its_menu() {
    use super::touch::finger;
    use eframe::egui::TouchPhase;
    let (mut h, _, texture, _sent) = chat(false, Vec2::new(390.0, 844.0));
    let pos = painted(&h, texture);
    finger(&mut h, TouchPhase::Start, pos);
    h.run_steps(8);
    finger(&mut h, TouchPhase::End, pos);
    h.step();
    assert!(shows(&h, t!("files.open_viewer")));
    assert!(shows(&h, t!("home.copy_message_id")));
}

/// Right-clicking the words or a link opens the message menu, like the blank part
/// of the row, while a left click on the link still follows it.
#[test]
fn right_clicking_text_or_a_link_opens_the_message_menu() {
    let (mut h, _, _, _sent) = chat(true, Vec2::new(1100.0, 700.0));
    let home = h.state_mut().as_mut().unwrap();
    let messages = &mut home.servers[0].chats.get_mut(&PROJECTS).unwrap().messages;
    messages.push(msg(4, ANN, "Notes at https://example.com/notes", vec![]));
    h.run_steps(4);
    let url = "https://example.com/notes";
    let opened = |h: &Harness<'_, Option<Home>>| {
        (h.output().platform_output.commands.iter())
            .any(|command| matches!(command, egui::OutputCommand::OpenUrl(open) if open.url == url))
    };

    let words = h.get_by_label("See you tomorrow").rect().center();
    right_click(&mut h, words);
    assert!(shows(&h, t!("common.edit")), "menu of my message");
    assert!(shows(&h, t!("home.copy_message_id")));
    for dark in [true, false] {
        let theme = if dark { "dark" } else { "light" };
        h.ctx.set_theme(if dark {
            egui::Theme::Dark
        } else {
            egui::Theme::Light
        });
        h.run_steps(2);
        assert!(shows(&h, t!("home.copy_message_id")));
        screenshot(&mut h, &format!("text-menu-{theme}-1100x700"));
    }
    close_menu(&mut h);
    assert!(!shows(&h, t!("home.copy_message_id")));

    let link = h.get_by_label(url).rect().center();
    right_click(&mut h, link);
    assert!(shows(&h, t!("home.copy_message_id")), "menu over a link");
    assert!(!shows(&h, t!("common.edit")), "Ann's message");
    assert!(!opened(&h), "a right click doesn't follow the link");
    close_menu(&mut h);

    let blank = h.get_by_label("Sounds good").rect().center() + egui::vec2(250.0, 0.0);
    right_click(&mut h, blank);
    assert!(
        shows(&h, t!("home.copy_message_id")),
        "menu on the blank part"
    );
    close_menu(&mut h);

    h.get_by_label(url).click();
    h.step();
    assert!(opened(&h), "a left click still follows the link");
    assert!(!shows(&h, t!("home.copy_message_id")));
}

#[test]
fn holding_message_text_on_a_phone_opens_the_message_menu() {
    use super::touch::finger;
    use eframe::egui::TouchPhase;
    let (mut h, _, _, _sent) = chat(false, Vec2::new(390.0, 844.0));
    let pos = h.get_by_label("Sounds good").rect().center();
    finger(&mut h, TouchPhase::Start, pos);
    h.run_steps(8);
    finger(&mut h, TouchPhase::End, pos);
    h.step();
    assert!(shows(&h, t!("home.copy_message_id")));
}
