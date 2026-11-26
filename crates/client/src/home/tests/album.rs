//! Several pictures in one message show as one album, and the viewer steps through them.

use super::gallery::{NOW, click, msg, photo};
use super::*;
use crate::net::Fetch;
use egui_kittest::kittest::Queryable;
use rusttalk_core::client::{Attachment, decode_frames, prepare_file};

struct Fixture {
    h: egui_kittest::Harness<'static, Home>,
    sent: tokio::sync::mpsc::UnboundedReceiver<Command>,
    /// Thumbnails and whole pictures by blob ID, to answer the requests for them.
    blobs: Vec<(Id, Vec<u8>)>,
    address: url::Url,
}

/// A chat with one message per album, oldest first: `name` and how many pictures.
/// The first album also carries a PDF, which stays a file row.
fn fixture(dark: bool, size: Vec2, albums: &[(&str, usize)]) -> Fixture {
    let (mut base, sent) = files_harness(dark);
    let mut home = base.state_mut().take().unwrap();
    let address = home.servers[0].address.clone();
    let mut blobs = Vec::new();
    let shapes = [(640, 480), (480, 640), (800, 400), (500, 500)];
    let mut messages = Vec::new();
    for (m, (name, count)) in albums.iter().enumerate() {
        let mut files: Vec<Attachment> = (0..*count)
            .map(|n| {
                let (w, h) = shapes[n % shapes.len()];
                let bytes = photo(w, h, (m * 7 + n) as u8);
                let (file, thumb) = prepare_file(
                    &format!("{name} {}.jpg", n + 1),
                    "image/jpeg",
                    &bytes,
                    &bytes,
                )
                .unwrap();
                blobs.push((file.thumbnail.as_ref().unwrap().id, thumb.unwrap()));
                blobs.push((file.file.id, bytes));
                file
            })
            .collect();
        if m == 0 {
            files.push(
                prepare_file("Notes.pdf", "application/pdf", &[0; 3000], &[])
                    .unwrap()
                    .0,
            );
        }
        let person = if m % 2 == 0 { ANN } else { ME };
        messages.push(msg(
            20 + m as u8,
            person,
            NOW - 3600 * (9 - m as i64),
            files,
            &address,
        ));
    }
    let chat = home.servers[0].chats.get_mut(&PROJECTS).unwrap();
    chat.messages = messages;
    chat.at_start = true;
    chat.loading = false;
    let mut h = egui_kittest::Harness::builder()
        .with_size(size)
        .with_context_setup(ui::install)
        .build_ui_state(
            |ui, home| {
                chat_pane(ui, home, 0, PROJECTS);
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
    let mut f = Fixture {
        h,
        sent,
        blobs,
        address,
    };
    f.load();
    f
}

impl Fixture {
    /// Answer every picture request so far; returns which whole pictures were asked for.
    fn load(&mut self) -> Vec<Id> {
        let mut full = Vec::new();
        for _ in 0..3 {
            let commands: Vec<_> = std::iter::from_fn(|| self.sent.try_recv().ok()).collect();
            for command in commands {
                let Command::Image(blob, fetch @ (Fetch::Thumbnail | Fetch::Full)) = command else {
                    continue;
                };
                if fetch == Fetch::Full {
                    full.push(blob.id);
                }
                let bytes = &self.blobs.iter().find(|(id, _)| *id == blob.id).unwrap().1;
                let frames = decode_frames(bytes, None, 64 * 1024 * 1024).unwrap();
                let frames = frames.into_iter().map(crate::net::frame_image).collect();
                let update = Update::Image(self.address.clone(), blob, fetch, Ok(frames));
                let ctx = self.h.ctx.clone();
                self.h.state_mut().apply(&ctx, update, 0.0);
            }
            self.h.run_steps(3);
        }
        full
    }

    fn cell(&self, label: &str) -> egui::Pos2 {
        self.h.get_by_label(label).rect().center()
    }

    /// The viewer's "2 / 6", for an album of up to six.
    fn position(&self) -> Option<String> {
        (1..=6)
            .flat_map(|total| (1..=total).map(move |at| format!("{at} / {total}")))
            .find(|text| self.h.query_by_label(text).is_some())
    }

    fn key(&mut self, key: egui::Key) {
        self.h.key_press(key);
        self.h.run_steps(3);
        self.load();
    }

    fn shot(&mut self, name: &str) {
        if let Some(folder) = std::env::var_os("RUSTTALK_ALBUM_SCREENSHOTS") {
            let folder = PathBuf::from(folder);
            std::fs::create_dir_all(&folder).unwrap();
            let image = self.h.render().expect("render");
            image.save(folder.join(format!("{name}.png"))).unwrap();
        }
    }
}

/// The grid for 2, 3, 4 and 6 pictures: square cells inside the album, never more than
/// four, the wide ones side by side.
#[test]
fn album_cells_fill_the_width_with_square_cells() {
    for (count, cells_shown, height) in [(2, 2, 199.0), (3, 3, 266.0), (4, 4, 400.0), (6, 4, 400.0)]
    {
        let (cells, tall) = crate::files::album_cells(count, 400.0);
        assert_eq!(cells.len(), cells_shown, "{count}");
        assert!((tall - height).abs() < 1.0, "{count}: {tall}");
        let outer = egui::Rect::from_min_size(egui::Pos2::ZERO, Vec2::new(400.0, tall));
        for cell in &cells {
            assert!(outer.expand(0.01).contains_rect(*cell), "{count}: {cell:?}");
            assert!(
                (cell.width() - cell.height()).abs() < 0.01,
                "{count}: {cell:?}"
            );
        }
        for (a, b) in cells.iter().zip(cells.iter().skip(1)) {
            assert!(a.intersect(*b).area() <= 0.0, "{count}: overlap");
        }
    }
    // Three: one big on the left, two small stacked on the right.
    let (cells, _) = crate::files::album_cells(3, 400.0);
    assert!(cells[0].width() > cells[1].width() * 1.9);
    assert_eq!(cells[1].left(), cells[2].left());
}

#[test]
fn albums_of_two_three_four_and_six_pictures() {
    for size in [Vec2::new(390.0, 844.0), Vec2::new(1100.0, 700.0)] {
        for (name, count) in [("Beach", 2), ("Forest", 3), ("Lake", 4), ("Hills", 6)] {
            let f = fixture(true, size, &[(name, count)]);
            let first = f.h.get_by_label(&format!("{name} 1.jpg")).rect();
            let width = first.width();
            assert!(width > 60.0, "{name}: {first:?}");
            assert!(
                f.h.ctx.content_rect().contains_rect(first),
                "{name}: {first:?}"
            );
            let cells = count.min(4);
            let mut right = first.right();
            for n in 2..=cells {
                let label = if n == 4 && count > 4 {
                    // The last cell tells how many more; the rest wait in the viewer.
                    format!("{name} 4.jpg · 3 more")
                } else {
                    format!("{name} {n}.jpg")
                };
                let cell = f.h.get_by_label(&label).rect();
                assert!(
                    cell.width() > 40.0 && cell.width() <= width + 1.0,
                    "{label}"
                );
                right = right.max(cell.right());
            }
            // The album fits the chat: never wider than the window.
            assert!(right <= size.x, "{name}: {right}");
            assert!(f.h.query_by_label_contains(&format!("{name} 5")).is_none());
            // Other files stay rows below the album.
            let pdf = f.h.get_by_label("Notes.pdf").rect();
            assert!(pdf.top() > first.bottom(), "{name}: {pdf:?}");
        }
    }
}

/// Screenshots of the four layouts at phone and desktop size, dark and light.
#[test]
fn album_screenshots() {
    for dark in [true, false] {
        let theme = if dark { "dark" } else { "light" };
        for size in [Vec2::new(390.0, 844.0), Vec2::new(1100.0, 700.0)] {
            let at = format!("{}x{}", size.x, size.y);
            for (which, albums) in [
                ("2-3", [("Beach", 2), ("Forest", 3)]),
                ("4-6", [("Lake", 4), ("Hills", 6)]),
            ] {
                let mut f = fixture(dark, size, &albums);
                f.h.get_by_label(&format!("{} 1.jpg", albums[1].0));
                f.shot(&format!("{theme}-albums-{which}-{at}"));
            }
        }
    }
}

/// The viewer steps through the message's pictures with the arrow keys, the on-screen
/// buttons and a swipe, and stops at the ends; Escape closes it.
#[test]
fn viewer_carousel_over_an_albums_pictures() {
    for dark in [true, false] {
        let theme = if dark { "dark" } else { "light" };
        for size in [Vec2::new(390.0, 844.0), Vec2::new(1100.0, 700.0)] {
            let at = format!("{}x{}", size.x, size.y);
            let mut f = fixture(dark, size, &[("Lake", 4), ("Hills", 6)]);
            let cell = f.cell("Hills 2.jpg");
            click(&mut f.h, cell);
            assert_eq!(f.load().len(), 1, "the whole picture loads");
            f.h.run_steps(3);
            assert_eq!(f.position().as_deref(), Some("2 / 6"));
            f.h.get_by_label(t!("gallery.previous"));
            f.shot(&format!("{theme}-viewer-{at}"));
            f.key(egui::Key::ArrowRight);
            assert_eq!(f.position().as_deref(), Some("3 / 6"));
            for _ in 0..4 {
                f.key(egui::Key::ArrowLeft);
            }
            // It stops at the first picture.
            assert_eq!(f.position().as_deref(), Some("1 / 6"));
            assert!(f.h.query_by_label(t!("gallery.previous")).is_none());
            f.h.get_by_label(t!("gallery.next")).click();
            f.h.run_steps(3);
            assert_eq!(f.position().as_deref(), Some("2 / 6"));
            for _ in 0..6 {
                f.key(egui::Key::ArrowRight);
            }
            // And at the last one, which the album's grid only counted.
            assert_eq!(f.position().as_deref(), Some("6 / 6"));
            f.h.get_by_label("Hills 6.jpg");
            assert!(f.h.query_by_label(t!("gallery.next")).is_none());
            f.key(egui::Key::Escape);
            assert!(f.position().is_none(), "Escape closes");

            // The "+3" cell opens at its own picture; a swipe left goes to the next.
            let cell = f.cell("Hills 4.jpg · 3 more");
            click(&mut f.h, cell);
            f.load();
            assert_eq!(f.position().as_deref(), Some("4 / 6"));
            let middle = f.h.ctx.content_rect().center();
            swipe(&mut f.h, middle, middle - Vec2::new(150.0, 0.0));
            assert_eq!(f.position().as_deref(), Some("5 / 6"));
            swipe(&mut f.h, middle, middle + Vec2::new(150.0, 0.0));
            assert_eq!(f.position().as_deref(), Some("4 / 6"));
            f.key(egui::Key::Escape);
        }
    }
}

/// On a desktop the buttons show while the pointer is over the window; on a touch
/// screen they always show.
#[test]
fn viewer_buttons_follow_the_pointer_on_desktop() {
    let mut f = fixture(true, Vec2::new(1100.0, 700.0), &[("Hills", 6)]);
    let cell = f.cell("Hills 2.jpg");
    click(&mut f.h, cell);
    f.load();
    f.h.get_by_label(t!("gallery.next"));
    f.h.event(egui::Event::PointerGone);
    f.h.run_steps(3);
    assert!(f.h.query_by_label(t!("gallery.next")).is_none());
    assert_eq!(f.position().as_deref(), Some("2 / 6"));
    f.h.event(egui::Event::Touch {
        device_id: egui::TouchDeviceId(0),
        id: egui::TouchId(0),
        phase: egui::TouchPhase::Cancel,
        pos: egui::pos2(500.0, 350.0),
        force: None,
    });
    f.h.event(egui::Event::PointerGone);
    f.h.run_steps(3);
    f.h.get_by_label(t!("gallery.next"));
}

fn swipe(h: &mut egui_kittest::Harness<'_, Home>, from: egui::Pos2, to: egui::Pos2) {
    let touch = |h: &mut egui_kittest::Harness<'_, Home>, pos, phase| {
        h.event(egui::Event::Touch {
            device_id: egui::TouchDeviceId(0),
            id: egui::TouchId(0),
            phase,
            pos,
            force: None,
        });
    };
    touch(h, from, egui::TouchPhase::Start);
    h.event(egui::Event::PointerMoved(from));
    h.event(egui::Event::PointerButton {
        pos: from,
        button: egui::PointerButton::Primary,
        pressed: true,
        modifiers: egui::Modifiers::NONE,
    });
    h.run_steps(1);
    for n in 1..=5 {
        let pos = from + (to - from) * (n as f32 / 5.0);
        touch(h, pos, egui::TouchPhase::Move);
        h.event(egui::Event::PointerMoved(pos));
        h.run_steps(1);
    }
    touch(h, to, egui::TouchPhase::End);
    h.event(egui::Event::PointerButton {
        pos: to,
        button: egui::PointerButton::Primary,
        pressed: false,
        modifiers: egui::Modifiers::NONE,
    });
    h.run_steps(3);
}
