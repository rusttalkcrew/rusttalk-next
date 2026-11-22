//! Emoji and stickers from other servers (#203). Members of a server can't read another
//! server's files, so the SENDER's app attaches the picture to the message as a small
//! file of its own type (`STICKER_TYPE` or `EMOJI_TYPE`), named `name.gif` or `.png`.
//! Apps that know the type draw it inline as a sticker or emoji and hide the file; older
//! apps show a small extra file. The file counts toward the server's file storage.
//! A received file is hostile: its type, size, frames and dimensions are checked here
//! before anything is decoded or shown. See docs/protocol.md.

use std::io::Cursor;

use anyhow::{Result, ensure};
use eframe::egui;
use image::{
    AnimationDecoder as _, Frame, ImageDecoder as _, ImageFormat, ImageReader, Limits,
    codecs::gif::GifDecoder,
};
use rusttalk_core::{client::Attachment, keys::IMAGE_PLAINTEXT};

use crate::server::{Id, Picture};

pub const STICKER_TYPE: &str = "application/vnd.rusttalk.sticker";
pub const EMOJI_TYPE: &str = "application/vnd.rusttalk.emoji";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Emoji,
    Sticker,
}

impl Kind {
    pub fn media_type(self) -> &'static str {
        match self {
            Kind::Emoji => EMOJI_TYPE,
            Kind::Sticker => STICKER_TYPE,
        }
    }

    pub fn sticker(self) -> bool {
        self == Kind::Sticker
    }
}

/// An emoji or sticker to attach to a message, as the sender's app has it.
#[derive(Clone)]
pub struct Inline {
    pub kind: Kind,
    pub name: String,
    pub bytes: std::sync::Arc<Vec<u8>>,
}

impl Inline {
    /// The name older apps show for the file.
    pub fn file_name(&self) -> String {
        let extension = match image::guess_format(&self.bytes) {
            Ok(ImageFormat::Gif) => "gif",
            Ok(ImageFormat::Jpeg) => "jpg",
            _ => "png",
        };
        format!("{}.{extension}", self.name)
    }
}

/// What a received file is, when it is a well-formed emoji or sticker file: its kind and
/// the name from its file name. Anything else, including a file with a thumbnail or of
/// the wrong size, stays an ordinary file.
pub fn of(file: &Attachment) -> Option<(Kind, String)> {
    let kind = match file.media_type.as_str() {
        STICKER_TYPE => Kind::Sticker,
        EMOJI_TYPE => Kind::Emoji,
        _ => return None,
    };
    let size = file.file.size as usize;
    if file.thumbnail.is_some() || size == 0 || size > IMAGE_PLAINTEXT {
        return None;
    }
    let (name, extension) = file.name.rsplit_once('.')?;
    (matches!(extension, "gif" | "png" | "jpg") && crate::emoji::name_ok(name))
        .then(|| (kind, name.to_owned()))
}

/// The one sticker a message is about: its file and name, when the text is just that
/// sticker's `:name:` (what older apps show). A message with other text keeps its
/// text and shows the sticker file as a file only for apps that don't know the type.
pub fn sticker_of<'a>(files: &'a [Attachment], text: &str) -> Option<(&'a Attachment, String)> {
    files.iter().find_map(|file| match of(file) {
        Some((Kind::Sticker, name)) if text.trim() == format!(":{name}:") => Some((file, name)),
        _ => None,
    })
}

/// Frames a received file may have, as apps save at most.
const MAX_FRAMES: usize = 100;

/// The most decoded bytes one received picture may hold: every frame at the largest size
/// for its kind, so whatever an app saves fits and nothing bigger does.
pub fn decoded_budget(kind: Kind) -> usize {
    let edge = crate::crop::received_edge(kind.sticker()) as usize;
    edge * edge * 4 * MAX_FRAMES
}

/// A received file's frames, only if it is a picture an app could have made for
/// `kind`: PNG, JPEG or GIF of at most `IMAGE_PLAINTEXT` bytes, no larger on a side than
/// apps save emoji or stickers (checked from the header, before any frame is decoded),
/// at most 100 frames and `decoded_budget` bytes decoded.
pub fn decode(bytes: &[u8], kind: Kind) -> Result<Vec<Frame>> {
    ensure!(bytes.len() <= IMAGE_PLAINTEXT, "file too large");
    let format = image::guess_format(bytes)?;
    ensure!(
        matches!(
            format,
            ImageFormat::Png | ImageFormat::Jpeg | ImageFormat::Gif
        ),
        "unsupported picture type"
    );
    let edge = crate::crop::received_edge(kind.sticker());
    let budget = decoded_budget(kind);
    let mut limits = Limits::default();
    limits.max_image_width = Some(edge);
    limits.max_image_height = Some(edge);
    limits.max_alloc = Some(budget as u64);
    if format != ImageFormat::Gif {
        let mut reader = ImageReader::with_format(Cursor::new(bytes), format);
        reader.limits(limits);
        return Ok(vec![Frame::new(reader.decode()?.into_rgba8())]);
    }
    let mut decoder = GifDecoder::new(Cursor::new(bytes))?;
    decoder.set_limits(limits)?;
    let (width, height) = decoder.dimensions();
    ensure!(width > 0 && height > 0, "GIF has an empty canvas");
    let frame_bytes = width as usize * height as usize * 4;
    let allowed = MAX_FRAMES.min(budget / frame_bytes);
    let mut frames = Vec::new();
    for frame in decoder.into_frames() {
        ensure!(frames.len() < allowed, "too many frames for its size");
        frames.push(frame?);
    }
    ensure!(!frames.is_empty(), "GIF has no frames");
    Ok(frames)
}

/// What a loaded picture holds in memory, to keep the total within a budget.
pub fn decoded_bytes(picture: &Picture) -> usize {
    (picture.frames.iter())
        .map(|(image, _)| image.pixels.len() * 4)
        .sum()
}

/// Whether the sender's app may attach this server picture: only what receivers accept.
pub fn sendable(bytes: &[u8], kind: Kind) -> bool {
    decode(bytes, kind).is_ok()
}

/// What a received file's loading came to, kept for the session in egui's memory next to
/// the textures (see `Files::sticker_loaded` for the bound).
#[derive(Clone)]
pub enum State {
    Loading,
    Ready(Picture),
    Failed,
}

fn key(blob: &Id) -> egui::Id {
    egui::Id::unique(("sticker file", *blob))
}

pub fn state(ctx: &egui::Context, blob: &Id) -> Option<State> {
    ctx.data(|data| data.get_temp(key(blob)))
}

pub fn set(ctx: &egui::Context, blob: &Id, state: State) {
    ctx.data_mut(|data| data.insert_temp(key(blob), state));
}

/// Drop a loaded picture, and its textures with it.
pub fn forget(ctx: &egui::Context, blob: &Id) {
    if let Some(picture) = picture(ctx, blob) {
        crate::home::forget_textures(ctx, &picture.key);
    }
    ctx.data_mut(|data| data.remove::<State>(key(blob)));
}

/// The picture of a received file, once loaded.
pub fn picture(ctx: &egui::Context, blob: &Id) -> Option<Picture> {
    match state(ctx, blob)? {
        State::Ready(picture) => Some(picture),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;

    use image::{Delay, Rgba, RgbaImage, codecs::gif::GifEncoder};
    use rusttalk_core::client::BlobRef;

    use super::*;

    fn blob(size: u32) -> BlobRef {
        let mut blob = BlobRef::for_tests([1; 32]);
        blob.size = size;
        blob
    }

    fn file(media_type: &str, name: &str, size: u32, thumbnail: bool) -> Attachment {
        Attachment {
            file: blob(size),
            name: name.into(),
            media_type: media_type.into(),
            thumbnail: thumbnail.then(|| blob(10)),
        }
    }

    #[test]
    fn only_well_formed_files_count() {
        let ok = file(STICKER_TYPE, "big_hug.gif", 5000, false);
        assert_eq!(of(&ok), Some((Kind::Sticker, "big_hug".into())));
        assert_eq!(
            of(&file(EMOJI_TYPE, "party.png", 5000, false)),
            Some((Kind::Emoji, "party".into()))
        );
        // Wrong type, a thumbnail, no size, too big, a bad name or extension.
        assert_eq!(of(&file("image/gif", "big_hug.gif", 5000, false)), None);
        assert_eq!(of(&file(STICKER_TYPE, "big_hug.gif", 5000, true)), None);
        assert_eq!(of(&file(STICKER_TYPE, "big_hug.gif", 0, false)), None);
        let too_big = IMAGE_PLAINTEXT as u32 + 1;
        assert_eq!(of(&file(STICKER_TYPE, "big_hug.gif", too_big, false)), None);
        assert_eq!(of(&file(STICKER_TYPE, "Big Hug.gif", 5000, false)), None);
        assert_eq!(of(&file(STICKER_TYPE, "x.gif", 5000, false)), None);
        assert_eq!(of(&file(STICKER_TYPE, "big_hug.exe", 5000, false)), None);
        assert_eq!(of(&file(STICKER_TYPE, "big_hug", 5000, false)), None);
    }

    #[test]
    fn a_sticker_message_needs_its_own_name_as_text() {
        let files = [
            file("image/png", "photo.png", 100, false),
            file(STICKER_TYPE, "big_hug.gif", 5000, false),
        ];
        let (found, name) = sticker_of(&files, " :big_hug: ").unwrap();
        assert_eq!(found.name, "big_hug.gif");
        assert_eq!(name, "big_hug");
        // Other text, or another sticker's name, is a normal message with a file.
        assert!(sticker_of(&files, "look :big_hug: here").is_none());
        assert!(sticker_of(&files, ":other:").is_none());
        // An emoji file is not a sticker.
        let emoji = [file(EMOJI_TYPE, "party.png", 100, false)];
        assert!(sticker_of(&emoji, ":party:").is_none());
    }

    fn png(side: u32) -> Vec<u8> {
        let mut out = Cursor::new(Vec::new());
        RgbaImage::from_pixel(side, side, Rgba([1, 2, 3, 255]))
            .write_to(&mut out, ImageFormat::Png)
            .unwrap();
        out.into_inner()
    }

    fn gif(side: u32, frames: usize) -> Vec<u8> {
        let mut bytes = Vec::new();
        let mut encoder = GifEncoder::new(&mut bytes);
        encoder
            .encode_frames((0..frames).map(|index| {
                Frame::from_parts(
                    RgbaImage::from_pixel(side, side, Rgba([index as u8, 9, 9, 255])),
                    0,
                    0,
                    Delay::from_numer_denom_ms(40, 1),
                )
            }))
            .unwrap();
        drop(encoder);
        bytes
    }

    #[test]
    fn received_pictures_are_held_to_the_limits_apps_save_with() {
        // Emoji are at most 128 px on a side, stickers 320.
        assert!(decode(&png(128), Kind::Emoji).is_ok());
        assert!(decode(&png(129), Kind::Emoji).is_err());
        assert!(decode(&png(320), Kind::Sticker).is_ok());
        assert!(decode(&png(321), Kind::Sticker).is_err());
        // Animation stays: every frame is checked, up to the frame cap.
        assert_eq!(decode(&gif(32, 5), Kind::Emoji).unwrap().len(), 5);
        assert!(decode(&gif(32, 101), Kind::Emoji).is_err());
        assert!(decode(&gif(129, 2), Kind::Emoji).is_err());
        // Not a picture, or too large to be one.
        assert!(decode(b"not a picture", Kind::Sticker).is_err());
        let mut big = png(8);
        big.resize(IMAGE_PLAINTEXT + 1, 0);
        assert!(decode(&big, Kind::Sticker).is_err());
        assert!(sendable(&png(64), Kind::Emoji));
        assert!(!sendable(&png(512), Kind::Emoji));
    }

    #[test]
    fn one_picture_never_holds_more_than_its_budget() {
        // The header says too large before any frame is decoded.
        assert!(decode(&gif(129, 1), Kind::Emoji).is_err());
        assert!(decode(&png(321), Kind::Sticker).is_err());
        // Frames at the largest size for the kind add up to the budget at most.
        assert_eq!(decoded_budget(Kind::Emoji), 128 * 128 * 4 * 100);
        assert_eq!(decoded_budget(Kind::Sticker), 320 * 320 * 4 * 100);
        assert_eq!(decode(&gif(128, 100), Kind::Emoji).unwrap().len(), 100);
        assert_eq!(decode(&gif(320, 20), Kind::Sticker).unwrap().len(), 20);
        let frames = decode(&gif(128, 100), Kind::Emoji).unwrap();
        let held = decoded_bytes(&Picture {
            key: [0; 32],
            frames: std::sync::Arc::new(
                frames
                    .into_iter()
                    .map(|f| {
                        let image = f.into_buffer();
                        let size = [image.width() as usize, image.height() as usize];
                        (
                            egui::ColorImage::from_rgba_unmultiplied(size, image.as_raw()),
                            40,
                        )
                    })
                    .collect(),
            ),
            bytes: std::sync::Arc::new(Vec::new()),
        });
        assert!(held <= decoded_budget(Kind::Emoji));
    }

    #[test]
    fn files_are_named_for_older_apps() {
        let inline = |bytes: Vec<u8>| Inline {
            kind: Kind::Sticker,
            name: "big_hug".into(),
            bytes: bytes.into(),
        };
        assert_eq!(inline(gif(8, 2)).file_name(), "big_hug.gif");
        assert_eq!(inline(png(8)).file_name(), "big_hug.png");
        // A name made for a file passes `of` when sent back.
        let named = file(STICKER_TYPE, &inline(png(8)).file_name(), 100, false);
        assert!(of(&named).is_some());
    }
}
