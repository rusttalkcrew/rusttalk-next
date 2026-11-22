//! Pasting pictures and copied files into the composer. The window only pastes
//! text, so Ctrl+V reaches the app as a key (main.rs turns the window's own
//! clipboard shortcuts off) and the composer reads the clipboard here. A
//! picture keeps the bytes the copying app offered, so a GIF stays animated;
//! copied files attach as they are. Without either, it becomes a normal text
//! paste again. Never log clipboard contents, addresses or file names here.

use std::{
    fs,
    io::{self, Read},
    path::PathBuf,
};

use base64::Engine;
use eframe::egui::{self, Event, Id, Key, RawInput, ViewportCommand};
use url::Url;

use crate::net::MAX_FILE;

/// Formats worth taking, best first: copied files, pictures that may move, a web
/// page's moving picture (browsers offer only a still copy of it as a picture),
/// then still pictures. Windows names are mapped to these in `windows::name`, macOS
/// pasteboard types in `macos::NAMES`.
const RANKED: [&str; 7] = [
    "text/uri-list",
    "x-special/gnome-copied-files",
    "image/gif",
    "image/webp",
    "text/html",
    "image/png",
    "image/jpeg",
];

pub enum Clip {
    Files(Vec<PathBuf>),
    /// The bytes and their file extension.
    Picture(Vec<u8>, &'static str),
}

/// A clipboard to read from.
trait Source {
    fn formats(&mut self) -> Vec<String>;
    fn get(&mut self, format: &str) -> Option<Vec<u8>>;
}

/// The offered formats worth trying, best first.
fn ranked(offered: &[String]) -> Vec<&'static str> {
    RANKED
        .into_iter()
        .filter(|format| offered.iter().any(|o| o.eq_ignore_ascii_case(format)))
        .collect()
}

fn read(source: &mut impl Source) -> Option<Clip> {
    for format in ranked(&source.formats()) {
        let Some(bytes) = source.get(format) else {
            continue;
        };
        match format {
            "text/uri-list" | "x-special/gnome-copied-files" => {
                let paths = file_paths(&bytes);
                if !paths.is_empty() {
                    return Some(Clip::Files(paths));
                }
            }
            "text/html" => {
                if let Some(clip) = web_picture(&bytes) {
                    return Some(clip);
                }
            }
            _ => {
                if let Some(extension) = kind(&bytes) {
                    return Some(Clip::Picture(bytes, extension));
                }
            }
        }
    }
    None
}

/// Local files in a `text/uri-list` (GNOME's own list starts with "copy" or "cut",
/// which is skipped like any other line that is not a file address).
pub(crate) fn file_paths(list: &[u8]) -> Vec<PathBuf> {
    String::from_utf8_lossy(list)
        .lines()
        .map(|line| line.trim_matches(|c: char| c.is_whitespace() || c == '\0'))
        .filter(|line| !line.starts_with('#'))
        .filter_map(|line| Url::parse(line).ok())
        .filter(|url| url.scheme() == "file")
        .filter_map(|url| url.to_file_path().ok())
        .collect()
}

/// The file type from the first bytes, so a mislabelled format can't name it.
fn kind(bytes: &[u8]) -> Option<&'static str> {
    if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
        Some("gif")
    } else if bytes.len() >= 12 && bytes.starts_with(b"RIFF") && &bytes[8..12] == b"WEBP" {
        Some("webp")
    } else if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        Some("png")
    } else if bytes.starts_with(&[0xFF, 0xD8, 0xFF]) {
        Some("jpg")
    } else {
        None
    }
}

/// The first `<img src>` of copied web page HTML.
fn image_source(html: &str) -> Option<String> {
    let lower = html.to_ascii_lowercase();
    let tag = lower.find("<img")?;
    let end = lower[tag..].find('>').map_or(html.len(), |end| tag + end);
    let (at, _) = lower[tag..end].match_indices("src=").find(|(at, _)| {
        lower[..tag + at]
            .chars()
            .next_back()
            .is_some_and(char::is_whitespace)
    })?;
    let value = &html[tag + at + 4..end];
    let value = match value.chars().next()? {
        quote @ ('"' | '\'') => value[1..].split(quote).next()?,
        _ => value.split_whitespace().next()?,
    };
    Some(value.replace("&amp;", "&"))
}

/// A moving picture (GIF or WebP) a copied web page shows. Still pictures come
/// from the clipboard's own picture formats instead, without a download.
fn web_picture(html: &[u8]) -> Option<Clip> {
    let source = image_source(&String::from_utf8_lossy(html))?;
    let bytes = if let Some(data) = source.strip_prefix("data:") {
        let (_, encoded) = data.split_once(";base64,")?;
        base64::engine::general_purpose::STANDARD
            .decode(encoded.trim())
            .ok()?
    } else {
        let url = Url::parse(&source).ok()?;
        let path = url.path().to_ascii_lowercase();
        if !path.ends_with(".gif") && !path.ends_with(".webp") {
            return None;
        }
        match url.scheme() {
            "file" => fs::read(url.to_file_path().ok()?).ok()?,
            "https" => download(&url)?,
            _ => return None,
        }
    };
    match kind(&bytes)? {
        extension @ ("gif" | "webp") => Some(Clip::Picture(bytes, extension)),
        _ => None,
    }
}

/// The picture a web page showed, from where the browser got it (see docs/privacy.md).
fn download(url: &Url) -> Option<Vec<u8>> {
    let fetched = (|| -> reqwest::Result<Vec<u8>> {
        let client = reqwest::blocking::Client::builder()
            .https_only(true)
            .timeout(std::time::Duration::from_secs(30))
            .build()?;
        let mut bytes = Vec::new();
        let reply = client.get(url.clone()).send()?.error_for_status()?;
        // One byte over the limit tells a too-big file apart.
        let _ = reply.take(MAX_FILE + 1).read_to_end(&mut bytes);
        Ok(bytes)
    })();
    match fetched {
        Ok(bytes) if bytes.len() as u64 <= MAX_FILE => Some(bytes),
        Ok(_) => {
            tracing::warn!("a pasted web picture is too big");
            None
        }
        Err(error) => {
            tracing::warn!(error = %error.without_url(), "downloading a pasted web picture failed");
            None
        }
    }
}

/// Write a pasted picture to a fresh `rusttalk-<id>` temporary folder, which
/// `files::discard` and `files::clear_opened` remove later.
fn save(bytes: &[u8], extension: &str) -> io::Result<PathBuf> {
    let id = rusttalk_core::keys::random_key().map_err(io::Error::other)?;
    let folder = crate::files::temp_folder(&id[..8]);
    fs::create_dir(&folder)?;
    let path = folder.join(format!("image.{extension}"));
    fs::write(&path, bytes)?;
    Ok(path)
}

/// The files to attach from the clipboard: copied files, or a saved picture.
/// `None` when it holds neither, so the paste is text.
pub fn files() -> Option<Vec<PathBuf>> {
    match system()? {
        Clip::Files(paths) => Some(paths),
        Clip::Picture(bytes, extension) => match save(&bytes, extension) {
            Ok(path) => Some(vec![path]),
            Err(error) => {
                tracing::warn!(%error, "saving a pasted picture failed");
                None
            }
        },
    }
}

const WANTED: &str = "paste-wanted";

/// Turn the clipboard keys back into what the window would have sent (see the
/// module comment), except paste, which waits for the composer: see `wanted`.
pub fn keys(ctx: &egui::Context, input: &mut RawInput) {
    input.events.retain_mut(|event| {
        let Event::Key {
            key,
            pressed: true,
            modifiers,
            ..
        } = *event
        else {
            return true;
        };
        let windows = cfg!(windows);
        if key == Key::Cut
            || (modifiers.command_only() && key == Key::X)
            || (windows && modifiers.shift && key == Key::Delete)
        {
            *event = Event::Cut;
        } else if key == Key::Copy
            || (modifiers.command_only() && key == Key::C)
            || (windows && modifiers.ctrl && key == Key::Insert)
        {
            *event = Event::Copy;
        } else if key == Key::Paste
            || (modifiers.command_only() && key == Key::V)
            || (windows && modifiers.shift && key == Key::Insert)
        {
            ctx.data_mut(|data| data.insert_temp(Id::unique(WANTED), true));
            return false;
        }
        true
    });
}

/// Whether the paste keys were pressed this frame; the caller then handles it.
pub fn wanted(ctx: &egui::Context) -> bool {
    ctx.data_mut(|data| data.remove_temp::<bool>(Id::unique(WANTED)))
        .unwrap_or(false)
}

/// A paste nobody took this frame is a text paste, as the window would have done.
pub fn text(ctx: &egui::Context) {
    if wanted(ctx) {
        ctx.send_viewport_cmd(ViewportCommand::RequestPaste);
    }
}

#[cfg(target_os = "linux")]
fn system() -> Option<Clip> {
    use wl_clipboard_rs::paste::{ClipboardType, Error, Seat, get_mime_types};
    if std::env::var_os("WAYLAND_DISPLAY").is_some() {
        match get_mime_types(ClipboardType::Regular, Seat::Unspecified) {
            Ok(formats) => return read(&mut linux::Wayland(formats.into_iter().collect())),
            Err(Error::NoSeats | Error::ClipboardEmpty | Error::NoMimeType) => return None,
            // GNOME has no clipboard reading protocol for apps: try its X11 clipboard.
            Err(error) => tracing::info!(%error, "Wayland clipboard unavailable"),
        }
    }
    let mut x11 = linux::X11::open()?;
    read(&mut x11)
}

#[cfg(target_os = "linux")]
mod linux {
    use std::{io::Read, time::Duration};

    use wl_clipboard_rs::paste::{ClipboardType, MimeType, Seat, get_contents};
    use x11_clipboard::Clipboard;
    use x11rb::protocol::xproto::ConnectionExt;

    use super::{MAX_FILE, Source};

    /// The offered formats.
    pub struct Wayland(pub Vec<String>);

    impl Source for Wayland {
        fn formats(&mut self) -> Vec<String> {
            self.0.clone()
        }

        fn get(&mut self, format: &str) -> Option<Vec<u8>> {
            let (pipe, _) = get_contents(
                ClipboardType::Regular,
                Seat::Unspecified,
                MimeType::Specific(format),
            )
            .inspect_err(|error| tracing::warn!(%error, "reading the clipboard failed"))
            .ok()?;
            let mut bytes = Vec::new();
            pipe.take(MAX_FILE).read_to_end(&mut bytes).ok()?;
            Some(bytes)
        }
    }

    pub struct X11(Clipboard);

    impl X11 {
        pub fn open() -> Option<Self> {
            Clipboard::new()
                .inspect_err(|error| tracing::warn!(%error, "opening the X11 clipboard failed"))
                .ok()
                .map(Self)
        }

        fn load(&self, target: u32) -> Option<Vec<u8>> {
            let atoms = &self.0.getter.atoms;
            (self.0)
                .load(
                    atoms.clipboard,
                    target,
                    atoms.property,
                    Duration::from_secs(3),
                )
                .inspect_err(|error| tracing::warn!(%error, "reading the clipboard failed"))
                .ok()
        }
    }

    impl Source for X11 {
        fn formats(&mut self) -> Vec<String> {
            let Some(targets) = self.load(self.0.getter.atoms.targets) else {
                return Vec::new();
            };
            let connection = &self.0.getter.connection;
            (targets.as_chunks::<4>().0.iter())
                .filter_map(|atom| {
                    let atom = u32::from_ne_bytes(*atom);
                    let reply = connection.get_atom_name(atom).ok()?.reply().ok()?;
                    String::from_utf8(reply.name).ok()
                })
                .collect()
        }

        fn get(&mut self, format: &str) -> Option<Vec<u8>> {
            let atom = self.0.getter.get_atom(format).ok()?;
            self.load(atom)
        }
    }
}

#[cfg(target_os = "macos")]
fn system() -> Option<Clip> {
    read(&mut macos::Pasteboard::general()).or_else(macos::tiff)
}

#[cfg(target_os = "macos")]
mod macos {
    use objc2::rc::Retained;
    use objc2_app_kit::{NSBitmapImageFileType, NSBitmapImageRep, NSPasteboard};
    use objc2_foundation::{NSDictionary, NSString};

    use super::{Clip, Source};

    /// The pasteboard types worth taking, by the names in `RANKED`.
    const NAMES: [(&str, &str); 6] = [
        ("public.file-url", "text/uri-list"),
        ("com.compuserve.gif", "image/gif"),
        ("org.webmproject.webp", "image/webp"),
        ("public.html", "text/html"),
        ("public.png", "image/png"),
        ("public.jpeg", "image/jpeg"),
    ];

    pub struct Pasteboard(Retained<NSPasteboard>);

    impl Pasteboard {
        pub fn general() -> Self {
            Self(NSPasteboard::generalPasteboard())
        }
    }

    impl Source for Pasteboard {
        fn formats(&mut self) -> Vec<String> {
            let Some(types) = self.0.types() else {
                return Vec::new();
            };
            (types.iter())
                .filter_map(|offered| {
                    let offered = offered.to_string();
                    NAMES.iter().find(|(id, _)| *id == offered)
                })
                .map(|(_, name)| (*name).to_owned())
                .collect()
        }

        fn get(&mut self, format: &str) -> Option<Vec<u8>> {
            let (id, _) = NAMES.iter().find(|(_, name)| *name == format)?;
            let id = NSString::from_str(id);
            if format == "text/uri-list" {
                // One pasteboard item per copied file, each with its file address.
                let list: Vec<String> = (self.0.pasteboardItems()?.iter())
                    .filter_map(|item| item.stringForType(&id))
                    .map(|url| url.to_string())
                    .collect();
                return Some(list.join("\r\n").into_bytes());
            }
            Some(self.0.dataForType(&id)?.to_vec())
        }
    }

    /// A picture offered only as TIFF, as many Mac apps copy them, as PNG.
    pub fn tiff() -> Option<Clip> {
        let tiff =
            NSPasteboard::generalPasteboard().dataForType(&NSString::from_str("public.tiff"))?;
        let image = NSBitmapImageRep::imageRepWithData(&tiff)?;
        let png = unsafe {
            image.representationUsingType_properties(
                NSBitmapImageFileType::PNG,
                &NSDictionary::new(),
            )
        }?;
        Some(Clip::Picture(png.to_vec(), "png"))
    }
}

// Phones: only text pastes for now (docs/mobile.md).
#[cfg(any(target_os = "android", target_os = "ios"))]
fn system() -> Option<Clip> {
    None
}

#[cfg(windows)]
fn system() -> Option<Clip> {
    let found = {
        let _open = clipboard_win::Clipboard::new_attempts(10)
            .inspect_err(|error| tracing::warn!(%error, "opening the clipboard failed"))
            .ok()?;
        read(&mut windows::Clipboard)
    };
    found.or_else(windows::decoded)
}

#[cfg(windows)]
mod windows {
    use std::{io::Cursor, path::PathBuf};

    use clipboard_win::{
        formats::CF_HDROP,
        raw::{EnumFormats, format_name_big, get_file_list_path, get_vec},
    };
    use url::Url;

    use super::{Clip, Source};

    /// The open clipboard (see `system`).
    pub struct Clipboard;

    /// A format's name as on Linux, for the ones worth taking.
    fn name(id: u32) -> Option<&'static str> {
        if id == CF_HDROP {
            return Some("text/uri-list");
        }
        Some(match format_name_big(id)?.as_str() {
            "PNG" | "image/png" => "image/png",
            "GIF" | "image/gif" => "image/gif",
            "image/webp" => "image/webp",
            "JFIF" | "image/jpeg" => "image/jpeg",
            "HTML Format" => "text/html",
            _ => return None,
        })
    }

    impl Source for Clipboard {
        fn formats(&mut self) -> Vec<String> {
            EnumFormats::new()
                .filter_map(name)
                .map(str::to_owned)
                .collect()
        }

        fn get(&mut self, format: &str) -> Option<Vec<u8>> {
            if format == "text/uri-list" {
                let mut paths: Vec<PathBuf> = Vec::new();
                get_file_list_path(&mut paths).ok()?;
                let list: Vec<String> = (paths.iter())
                    .filter_map(|path| Url::from_file_path(path).ok())
                    .map(String::from)
                    .collect();
                return Some(list.join("\r\n").into_bytes());
            }
            let id = EnumFormats::new().find(|id| name(*id) == Some(format))?;
            let mut bytes = Vec::new();
            get_vec(id, &mut bytes).ok()?;
            Some(bytes)
        }
    }

    /// A picture offered only as a bitmap (no encoded format), as PNG.
    pub fn decoded() -> Option<Clip> {
        let image = arboard::Clipboard::new().ok()?.get_image().ok()?;
        let image = image::RgbaImage::from_raw(
            image.width as u32,
            image.height as u32,
            image.bytes.into_owned(),
        )?;
        let mut png = Cursor::new(Vec::new());
        image
            .write_to(&mut png, image::ImageFormat::Png)
            .inspect_err(|error| tracing::warn!(%error, "encoding a pasted picture failed"))
            .ok()?;
        Some(Clip::Picture(png.into_inner(), "png"))
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;

    const GIF: &[u8] = b"GIF89a\x01\x00\x01\x00\x00\x00\x00;";
    const PNG: &[u8] = b"\x89PNG\r\n\x1a\n rest";

    struct Fake(HashMap<&'static str, Vec<u8>>);

    impl Source for Fake {
        fn formats(&mut self) -> Vec<String> {
            self.0.keys().map(|format| format.to_string()).collect()
        }

        fn get(&mut self, format: &str) -> Option<Vec<u8>> {
            self.0.get(format).cloned()
        }
    }

    fn fake(formats: &[(&'static str, &[u8])]) -> Fake {
        Fake(formats.iter().map(|(f, b)| (*f, b.to_vec())).collect())
    }

    #[test]
    fn ranks_files_then_moving_then_still_pictures() {
        let offered: Vec<String> = [
            "text/plain",
            "image/jpeg",
            "image/png",
            "TEXT/HTML",
            "image/webp",
            "image/gif",
            "text/uri-list",
        ]
        .map(String::from)
        .to_vec();
        assert_eq!(
            ranked(&offered),
            [
                "text/uri-list",
                "image/gif",
                "image/webp",
                "text/html",
                "image/png",
                "image/jpeg"
            ]
        );
        assert!(ranked(&["text/plain".into()]).is_empty());
    }

    #[test]
    fn keeps_the_gif_over_a_still_copy() {
        let mut source = fake(&[("image/png", PNG), ("image/gif", GIF)]);
        let Some(Clip::Picture(bytes, "gif")) = read(&mut source) else {
            panic!("expected the GIF");
        };
        assert_eq!(bytes, GIF);
    }

    #[test]
    fn takes_a_web_pages_gif_over_its_still_copy() {
        let html = format!(
            "<meta charset='utf-8'><img alt=\"x\" data-src=\"no\" src=\"data:image/gif;base64,{}\">",
            base64::engine::general_purpose::STANDARD.encode(GIF)
        );
        let mut source = fake(&[("image/png", PNG), ("text/html", html.as_bytes())]);
        assert!(matches!(read(&mut source), Some(Clip::Picture(_, "gif"))));
    }

    #[test]
    fn a_still_web_picture_uses_the_offered_png() {
        let html = b"<img src=\"https://example.invalid/a.png\">";
        let mut source = fake(&[("text/html", html), ("image/png", PNG)]);
        assert!(matches!(read(&mut source), Some(Clip::Picture(_, "png"))));
    }

    #[test]
    fn copied_files_win_and_text_alone_is_nothing() {
        let path = std::env::temp_dir().join("a b.gif");
        let list = format!("copy\n{}\n", Url::from_file_path(&path).unwrap());
        let mut source = fake(&[
            ("x-special/gnome-copied-files", list.as_bytes()),
            ("image/png", PNG),
        ]);
        let Some(Clip::Files(paths)) = read(&mut source) else {
            panic!("expected files");
        };
        assert_eq!(paths, [path]);
        assert!(read(&mut fake(&[("text/plain", b"hi")])).is_none());
        // A link copied from a browser is not a file: fall through to text.
        assert!(read(&mut fake(&[("text/uri-list", b"https://example.org/\r\n")])).is_none());
    }

    #[test]
    fn mislabelled_bytes_are_skipped() {
        let mut source = fake(&[("image/gif", b"not a picture"), ("image/png", PNG)]);
        assert!(matches!(read(&mut source), Some(Clip::Picture(_, "png"))));
    }

    #[test]
    fn parses_uri_lists() {
        // Windows lists are built the same way from its copied file paths.
        let (one, two) = (
            std::env::temp_dir().join("one.png"),
            std::env::temp_dir().join("two%.txt"),
        );
        let [first, second] = [&one, &two].map(|path| Url::from_file_path(path).unwrap());
        assert!(second.as_str().ends_with("two%25.txt"));
        let list = format!("# comment\r\n{first}\r\nhttps://example.org/x\r\n\r\n{second}\0");
        assert_eq!(file_paths(list.as_bytes()), [one, two]);
    }

    #[test]
    fn finds_image_sources() {
        assert_eq!(
            image_source("<p><IMG\nsrc='https://e.org/a.gif?x=1&amp;y=2' alt=a></p>").as_deref(),
            Some("https://e.org/a.gif?x=1&y=2")
        );
        assert_eq!(
            image_source("<img src=https://e.org/b.webp>").as_deref(),
            Some("https://e.org/b.webp")
        );
        assert_eq!(image_source("<img data-src=\"x.gif\">"), None);
        assert_eq!(image_source("<p>no picture</p>"), None);
    }

    #[test]
    fn clipboard_keys_copy_and_cut_but_paste_waits() {
        let ctx = egui::Context::default();
        let key = |key, modifiers| Event::Key {
            key,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers,
        };
        let mut input = RawInput {
            events: vec![
                key(Key::C, egui::Modifiers::COMMAND),
                key(Key::X, egui::Modifiers::COMMAND),
                key(Key::V, egui::Modifiers::COMMAND),
                key(Key::V, egui::Modifiers::NONE),
            ],
            ..RawInput::default()
        };
        keys(&ctx, &mut input);
        assert_eq!(
            input.events,
            [Event::Copy, Event::Cut, key(Key::V, egui::Modifiers::NONE)]
        );
        assert!(wanted(&ctx));
        assert!(!wanted(&ctx), "taken once");
    }

    #[test]
    fn sniffs_picture_types() {
        assert_eq!(kind(GIF), Some("gif"));
        assert_eq!(kind(b"RIFF\0\0\0\0WEBPVP8 "), Some("webp"));
        assert_eq!(kind(PNG), Some("png"));
        assert_eq!(kind(&[0xFF, 0xD8, 0xFF, 0xE0]), Some("jpg"));
        assert_eq!(kind(b"BM"), None);
    }

    #[test]
    fn saves_pictures_in_a_removable_temporary_folder() {
        let path = save(GIF, "gif").unwrap();
        assert_eq!(fs::read(&path).unwrap(), GIF);
        assert_eq!(path.file_name().unwrap(), "image.gif");
        crate::files::discard(&path);
        assert!(!path.parent().unwrap().exists());
    }
}
