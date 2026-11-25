use super::*;
use crate::net::Fetch;
use egui_kittest::kittest::{NodeT, Queryable};
use rusttalk_core::client::{Attachment, decode_frames, prepare_file};

/// A PNG of `w` by `h` with a soft two-colour gradient, so tiles look like photos.
fn photo(w: u32, h: u32, seed: u8) -> Vec<u8> {
    let image = image::RgbaImage::from_fn(w, h, |x, y| {
        let (fx, fy) = (x as f32 / w as f32, y as f32 / h as f32);
        let s = f32::from(seed) * 37.0;
        let channel = |phase: f32| ((phase + s).to_radians().sin() * 0.5 + 0.5) * 255.0;
        image::Rgba([
            (channel(fx * 120.0) * (1.0 - fy * 0.4)) as u8,
            (channel(90.0 + fy * 140.0)) as u8,
            (channel(200.0 + (fx + fy) * 80.0) * 0.9) as u8,
            255,
        ])
    });
    let mut png = std::io::Cursor::new(Vec::new());
    image.write_to(&mut png, image::ImageFormat::Png).unwrap();
    png.into_inner()
}

fn msg(id: u8, person: Id, at: i64, files: Vec<Attachment>, address: &url::Url) -> Msg {
    Msg {
        id: [id; 32],
        person,
        at,
        body: Body::Text(String::new()),
        backfilled: false,
        reply_to: None,
        quoted_author: None,
        edit_of: None,
        files,
        live: None,
        sticker: None,
        replicas: vec![address.clone()],
        copy_files: vec![],
    }
}

struct Fixture {
    h: egui_kittest::Harness<'static, Home>,
    sent: tokio::sync::mpsc::UnboundedReceiver<Command>,
    /// Thumbnails by blob ID, to answer the requests for them.
    thumbs: Vec<(Id, Vec<u8>)>,
    address: url::Url,
    /// The newest picture, the one after it, and the video.
    newest: Attachment,
    second: Attachment,
    video: Attachment,
}

/// November 20, 2026, noon UTC: the newest message.
const NOW: i64 = 1_795_176_000;
const DAY: i64 = 86_400;

/// A channel (or DM) with pictures of several shapes, a GIF, a video, an SVG and a PDF
/// over two months, and older history still on the server.
fn fixture(dark: bool, size: Vec2, dm: bool) -> Fixture {
    let (mut base, sent) = files_harness(dark);
    let mut home = base.state_mut().take().unwrap();
    let address = home.servers[0].address.clone();
    let mut thumbs = Vec::new();
    let mut file = |name: &str, media_type: &str, picture: Option<Vec<u8>>| {
        let bytes = picture.clone().unwrap_or_else(|| vec![0; 3000]);
        let (file, thumb) =
            prepare_file(name, media_type, &bytes, picture.as_deref().unwrap_or(&[])).unwrap();
        if let (Some(blob), Some(thumb)) = (&file.thumbnail, thumb) {
            thumbs.push((blob.id, thumb));
        }
        file
    };
    let shapes = [(640, 480), (480, 640), (800, 400), (500, 500)];
    let mut messages = Vec::new();
    let mut pictures = Vec::new();
    for n in 0..14u8 {
        let (w, h) = shapes[usize::from(n) % shapes.len()];
        let name = format!("Photo {}.jpg", n + 1);
        pictures.push(file(&name, "image/jpeg", Some(photo(w, h, n))));
    }
    let gif = file("Party.gif", "image/gif", Some(photo(320, 240, 20)));
    let video = file("Clip.mp4", "video/mp4", None);
    let svg = file("Logo.svg", "image/svg+xml", None);
    let pdf = file("Notes.pdf", "application/pdf", None);
    // Oldest first, as the chat keeps them; a message may carry several files.
    let mut at = NOW - 40 * DAY;
    let mut id = 10;
    let mut push = |files: Vec<Attachment>, person: Id, step: i64| {
        messages.push(msg(id, person, at, files, &address));
        id += 1;
        at += step;
    };
    push(vec![svg.clone(), pdf.clone()], ANN, 3 * DAY);
    for (n, picture) in pictures.iter().enumerate().skip(2).rev() {
        let person = if n % 3 == 0 { ME } else { ANN };
        push(vec![picture.clone()], person, 2 * DAY + 3600);
    }
    push(vec![video.clone(), gif.clone()], ME, DAY);
    push(vec![pictures[0].clone(), pictures[1].clone()], ANN, 0);
    if dm {
        let mut dir = home.servers[0].dir.clone();
        dir.dms.push(crate::server::Dm {
            channel: PROJECTS,
            peer: ANN,
        });
        dir.categories.clear();
        home.apply(
            &base.ctx,
            Update::Cached(address.clone(), Box::new(dir), vec![(PROJECTS, messages)]),
            0.0,
        );
    } else {
        let chat = home.servers[0].chats.get_mut(&PROJECTS).unwrap();
        chat.messages = messages;
        chat.oldest = Some([10; 32]);
        chat.at_start = false;
        chat.loading = false;
    }
    let mut h = egui_kittest::Harness::builder()
        .with_size(size)
        .with_context_setup(ui::install)
        .build_ui_state(
            move |ui, home| {
                if dm {
                    dm_pane(ui, home, ANN);
                } else {
                    chat_pane(ui, home, 0, PROJECTS);
                }
                files::viewer(ui.ctx(), &mut home.files);
            },
            home,
        );
    h.ctx.set_theme(if dark {
        egui::Theme::Dark
    } else {
        egui::Theme::Light
    });
    h.run_steps(3);
    Fixture {
        h,
        sent,
        thumbs,
        address,
        newest: pictures[0].clone(),
        second: pictures[1].clone(),
        video,
    }
}

impl Fixture {
    fn commands(&mut self) -> Vec<Command> {
        std::iter::from_fn(|| self.sent.try_recv().ok()).collect()
    }

    fn open_gallery(&mut self) {
        self.h.get_by_label(t!("gallery.show")).click();
        self.h.run_steps(3);
    }

    /// Answer every thumbnail request so far; returns the other commands.
    fn load_thumbnails(&mut self) -> Vec<Command> {
        let mut rest = Vec::new();
        for _ in 0..3 {
            for command in self.commands() {
                match command {
                    Command::Image(blob, Fetch::Thumbnail) => {
                        let bytes = &self.thumbs.iter().find(|(id, _)| *id == blob.id).unwrap().1;
                        let frames = decode_frames(bytes, None, 64 * 1024 * 1024).unwrap();
                        let ctx = self.h.ctx.clone();
                        let update = Update::Image(
                            self.address.clone(),
                            blob,
                            Fetch::Thumbnail,
                            Ok(frames.into_iter().map(crate::net::frame_image).collect()),
                        );
                        self.h.state_mut().apply(&ctx, update, 0.0);
                    }
                    other => rest.push(other),
                }
            }
            self.h.run_steps(3);
        }
        rest
    }

    fn shot(&mut self, name: &str) {
        if let Some(folder) = std::env::var_os("RUSTTALK_GALLERY_SCREENSHOTS") {
            let folder = PathBuf::from(folder);
            std::fs::create_dir_all(&folder).unwrap();
            let image = self.h.render().expect("render");
            image.save(folder.join(format!("{name}.png"))).unwrap();
        }
    }
}

fn tile_labels(h: &egui_kittest::Harness<'_, Home>) -> Vec<String> {
    let caption = " · ";
    ["Photo", "Party", "Clip", "Logo", "Notes", "Old"]
        .iter()
        .flat_map(|name| h.query_all_by_label_contains(name).collect::<Vec<_>>())
        .filter_map(|node| node.accesskit_node().label())
        .filter(|label| label.contains(caption))
        .collect()
}

#[test]
fn gallery_button_shows_pictures_and_videos_and_pages_older_history() {
    for dark in [true, false] {
        let mut f = fixture(dark, Vec2::new(1100.0, 700.0), false);
        assert!(f.h.query_by_label(t!("gallery.hide")).is_none());
        f.open_gallery();
        assert!(crate::home::gallery::shown(&f.h.ctx, PROJECTS));
        let rest = f.load_thumbnails();
        let labels = tile_labels(&f.h);
        for name in [
            "Photo 1.jpg",
            "Photo 14.jpg",
            "Party.gif",
            "Clip.mp4",
            "Logo.svg",
        ] {
            assert!(
                labels.iter().any(|label| label.starts_with(name)),
                "{name} in {labels:?}"
            );
        }
        assert!(!labels.iter().any(|label| label.starts_with("Notes.pdf")));
        // Everything loaded fits: the gallery asks for the page before, like the chat.
        assert!(
            rest.iter().any(
                |c| matches!(c, Command::Older(channel, before) if *channel == PROJECTS && *before == [10; 32])
            ),
            "older page requested"
        );
        f.shot(&format!("{}-1100x700", if dark { "dark" } else { "light" }));
        // The last page arrives: its picture shows and paging stops.
        let (older, thumb) = file_from_page();
        f.thumbs.push((older.thumbnail.as_ref().unwrap().id, thumb));
        let ctx = f.h.ctx.clone();
        let page = Update::Messages {
            server: f.address.clone(),
            channel: PROJECTS,
            items: vec![msg(5, ANN, NOW - 80 * DAY, vec![older], &f.address)],
            older: true,
            page: Some((true, Some([5; 32]))),
        };
        f.h.state_mut().apply(&ctx, page, 0.0);
        f.h.run_steps(3);
        assert!(
            tile_labels(&f.h)
                .iter()
                .any(|l| l.starts_with("Old photo.png"))
        );
        let rest = f.load_thumbnails();
        assert!(!rest.iter().any(|c| matches!(c, Command::Older(..))));
        // Hiding it brings the chat back.
        f.h.get_by_label(t!("gallery.hide")).click();
        f.h.run_steps(3);
        assert!(!crate::home::gallery::shown(&f.h.ctx, PROJECTS));
    }
}

fn file_from_page() -> (Attachment, Vec<u8>) {
    let picture = photo(300, 200, 9);
    let (file, thumb) = prepare_file("Old photo.png", "image/png", &picture, &picture).unwrap();
    (file, thumb.unwrap())
}

#[test]
fn gallery_on_a_phone_and_in_a_dm() {
    for dark in [true, false] {
        let mut f = fixture(dark, Vec2::new(390.0, 844.0), false);
        f.open_gallery();
        f.load_thumbnails();
        assert!(tile_labels(&f.h).iter().any(|l| l.starts_with("Clip.mp4")));
        f.shot(&format!("{}-390x844", if dark { "dark" } else { "light" }));
        let photo = f.h.get_by_label_contains("Photo 2.jpg").rect().center();
        click(&mut f.h, photo);
        f.h.get_by_label(t!("gallery.previous"));
        f.h.get_by_label(t!("gallery.next"));
        f.shot(&format!(
            "{}-viewer-390x844",
            if dark { "dark" } else { "light" }
        ));

        let mut f = fixture(dark, Vec2::new(1100.0, 700.0), true);
        f.open_gallery();
        assert!(crate::home::gallery::shown(&f.h.ctx, ANN));
        f.load_thumbnails();
        assert!(
            tile_labels(&f.h)
                .iter()
                .any(|l| l.starts_with("Photo 1.jpg"))
        );
    }
}

/// The grid lines: a title per month, rows of `columns`, newest first.
#[test]
fn entries_are_newest_first_per_month_without_repeats() {
    let address: url::Url = "https://friends.example.org/".parse().unwrap();
    let picture = |name: &str| {
        prepare_file(name, "image/png", &[1; 10], &photo(4, 4, 1))
            .unwrap()
            .0
    };
    let (a, b, c) = (picture("a.png"), picture("b.png"), picture("c.png"));
    let card = Attachment {
        media_type: crate::link_cards::MEDIA_TYPE.into(),
        ..picture("card")
    };
    let messages = vec![
        msg(1, ANN, NOW - 40 * DAY, vec![a.clone()], &address),
        msg(
            2,
            ANN,
            NOW - DAY,
            vec![b.clone(), c.clone(), card],
            &address,
        ),
        // A page's next version carries the same picture again.
        msg(3, ANN, NOW, vec![a.clone()], &address),
    ];
    let entries = crate::home::gallery::entries(&messages);
    let ids: Vec<_> = entries
        .iter()
        .map(|e| messages[e.msg].files[e.file].file.id)
        .collect();
    assert_eq!(ids, [a.file.id, b.file.id, c.file.id]);
    let lines = crate::home::gallery::lines(&messages, &entries, 2);
    assert_eq!(lines.len(), 3, "{lines:?}");
    assert!(matches!(lines[0], crate::home::gallery::Line::Month(_)));
    assert_eq!(lines[1], crate::home::gallery::Line::Tiles(0..2));
    assert_eq!(lines[2], crate::home::gallery::Line::Tiles(2..3));
}

#[test]
fn viewer_steps_through_the_gallery_and_jumps_to_the_message() {
    let mut f = fixture(true, Vec2::new(1100.0, 700.0), false);
    f.open_gallery();
    f.load_thumbnails();
    let tile = |f: &Fixture, name: &str| {
        f.h.query_all_by_label_contains(name)
            .find(|n| {
                n.accesskit_node()
                    .label()
                    .is_some_and(|l| l.contains(" · "))
            })
            .unwrap()
            .rect()
            .center()
    };
    // A video opens in its own app, downloaded first.
    let at = tile(&f, "Clip.mp4");
    click(&mut f.h, at);
    let commands = f.commands();
    assert!(
        commands
            .iter()
            .any(|c| matches!(c, Command::Save(blob, _) if blob.id == f.video.file.id)),
        "video saved to open"
    );
    let at = tile(&f, "Photo 1.jpg");
    click(&mut f.h, at);
    let full = |commands: Vec<Command>| -> Vec<Id> {
        commands
            .into_iter()
            .filter_map(|c| match c {
                Command::Image(blob, Fetch::Full) => Some(blob.id),
                _ => None,
            })
            .collect()
    };
    assert_eq!(full(f.commands()), [f.newest.file.id]);
    f.h.get_by_label("Photo 1.jpg");
    assert!(f.h.query_by_label(t!("gallery.previous")).is_none());
    f.shot("dark-viewer-1100x700");
    f.h.get_by_label(t!("gallery.next")).click();
    f.h.run_steps(3);
    assert_eq!(full(f.commands()), [f.second.file.id]);
    f.h.get_by_label("Photo 2.jpg");
    f.h.key_press(egui::Key::ArrowLeft);
    f.h.run_steps(3);
    f.h.get_by_label("Photo 1.jpg");
    // Back in the chat, at the message that shared it.
    f.h.get_by_label(t!("gallery.jump")).click();
    f.h.run_steps(3);
    assert!(!crate::home::gallery::shown(&f.h.ctx, PROJECTS));
    assert!(f.h.query_by_label(t!("gallery.next")).is_none());
    // An older picture: the chat scrolls up to its message.
    f.open_gallery();
    f.load_thumbnails();
    let at = tile(&f, "Photo 12.jpg");
    click(&mut f.h, at);
    f.h.get_by_label(t!("gallery.jump")).click();
    f.h.run_steps(8);
    let chat = &f.h.state().servers[0].chats[&PROJECTS];
    assert!(chat.jump_to.is_none());
    let bottom = chat.content_height - 700.0;
    assert!(
        chat.scroll_offset < bottom - 200.0,
        "{} of {}",
        chat.scroll_offset,
        chat.content_height
    );
}

fn click(h: &mut egui_kittest::Harness<'_, Home>, pos: egui::Pos2) {
    h.event(egui::Event::PointerMoved(pos));
    h.run_steps(1);
    for pressed in [true, false] {
        h.event(egui::Event::PointerButton {
            pos,
            button: egui::PointerButton::Primary,
            pressed,
            modifiers: egui::Modifiers::NONE,
        });
        h.run_steps(1);
    }
    h.run_steps(2);
}
