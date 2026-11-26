//! The crop step after picking a profile picture or server icon: move and zoom a
//! square over the picture, then save it scaled and re-encoded to fit the limits.

use std::io::Cursor;

use anyhow::{Result, ensure};
use eframe::egui::{
    self, Color32, ColorImage, Context, CornerRadius, Rect, Role, Sense, Slider, Stroke,
    StrokeKind, TextureHandle, TextureOptions, Ui, Vec2, pos2,
};
use image::{
    Delay, DynamicImage, Frame, ImageFormat, ImageReader, Limits, RgbaImage,
    codecs::{
        gif::{GifEncoder, Repeat},
        jpeg::JpegEncoder,
    },
    imageops::FilterType,
};
use rusttalk_core::{client::decode_frames, keys::IMAGE_PLAINTEXT};

use crate::ui;

/// The largest picture side that can be saved.
const EDGE: u32 = 512;
/// Pictures are scaled down to this on opening: plenty for a 512 crop, and it keeps
/// the texture and the memory small for large photos.
const WORKING_EDGE: u32 = 2048;
/// Decode limits for a picked file, so a hostile file can't eat all memory.
const OPEN_EDGE: u32 = 16_384;
const OPEN_ALLOC: u64 = 512 * 1024 * 1024;
const MAX_ZOOM: f32 = 8.0;
/// JPEG qualities tried in turn when the PNG is too large.
const QUALITIES: [u8; 6] = [90, 82, 74, 66, 58, 50];
const VIEW: f32 = 300.0;

/// Animations are saved at most this large (a still picture keeps `EDGE`): every frame
/// counts, and the file has to fit the same size limit.
const ANIMATED_EDGE: u32 = 256;
/// Receivers play at most this many frames.
const MAX_FRAMES: usize = 100;
/// An animation's frames are scaled to this on opening: there are many of them.
const ANIMATION_WORKING_EDGE: u32 = 512;
/// Decoded frames of an opened animation together stay within this many bytes.
const ANIMATION_BUDGET: usize = 256 * 1024 * 1024;
/// Animations stop shrinking (fewer frames, then smaller) at this many frames.
const MIN_FRAMES: usize = 12;

/// Server emoji and stickers are saved at most this large: plenty for how they show.
const EMOJI_EDGE: u32 = 128;
const STICKER_EDGE: u32 = 320;

/// What the picture is for.
#[derive(Clone)]
pub enum Target {
    Avatar,
    Icon(url::Url),
    /// A picture picked for a server emoji or (`sticker`) sticker, cropped again
    /// before it is uploaded: `slot` is its place in the upload list.
    Emoji {
        sticker: bool,
        slot: u64,
    },
}

/// The square crop, in picture pixels: its centre and the zoom (1 shows the
/// whole short side).
#[derive(Clone, Copy, Debug, PartialEq)]
struct Square {
    width: f32,
    height: f32,
    center: Vec2,
    zoom: f32,
}

impl Square {
    fn new(width: u32, height: u32) -> Self {
        let (width, height) = (width as f32, height as f32);
        Self {
            width,
            height,
            center: Vec2::new(width / 2.0, height / 2.0),
            zoom: 1.0,
        }
    }

    /// The side of the crop: the short side over the zoom, at least one pixel.
    fn side(&self) -> f32 {
        (self.width.min(self.height) / self.zoom).max(1.0)
    }

    /// Keep the zoom in range and the square inside the picture.
    fn clamp(&mut self) {
        self.zoom = self.zoom.clamp(1.0, MAX_ZOOM);
        let half = self.side() / 2.0;
        self.center.x = self.center.x.clamp(half, self.width - half);
        self.center.y = self.center.y.clamp(half, self.height - half);
    }

    /// Drag the picture by `delta` screen points in a view `view` points wide.
    fn pan(&mut self, delta: Vec2, view: f32) {
        self.center -= delta * self.side() / view;
        self.clamp();
    }

    fn zoom_to(&mut self, zoom: f32) {
        self.zoom = zoom;
        self.clamp();
    }

    /// The crop in whole pixels: x, y and side.
    fn pixels(&self) -> (u32, u32, u32) {
        let side = self.side().round().max(1.0) as u32;
        let half = side as f32 / 2.0;
        let x = (self.center.x - half).round().max(0.0) as u32;
        let y = (self.center.y - half).round().max(0.0) as u32;
        let x = x.min((self.width as u32).saturating_sub(side));
        let y = y.min((self.height as u32).saturating_sub(side));
        (x, y, side)
    }

    /// The crop as texture coordinates.
    fn uv(&self) -> Rect {
        let half = self.side() / 2.0;
        Rect::from_min_max(
            pos2(
                (self.center.x - half) / self.width,
                (self.center.y - half) / self.height,
            ),
            pos2(
                (self.center.x + half) / self.width,
                (self.center.y + half) / self.height,
            ),
        )
    }
}

pub struct Crop {
    pub target: Target,
    image: RgbaImage,
    square: Square,
    texture: Option<TextureHandle>,
    /// Every frame of an animated GIF or WebP (empty for a still picture), scaled to
    /// the same size as `image`, the first one. The crop applies to all of them.
    frames: Vec<Frame>,
    error: Option<&'static str>,
}

/// What the crop dialog asks for this frame.
pub enum Outcome {
    Open,
    Cancel,
    Save(Vec<u8>),
}

impl Crop {
    /// Read and decode a picked file; the error is for people.
    pub fn open(target: Target, path: &std::path::Path) -> Result<Self, &'static str> {
        let bytes = std::fs::read(path).map_err(|error| {
            tracing::warn!(%error, "reading a picture failed");
            t!("crop.open_failed")
        })?;
        let (image, frames) = decode(&bytes).map_err(|error| {
            tracing::info!(error = %format_args!("{error:#}"), "picture refused");
            if other_picture(&bytes) {
                t!("crop.wrong_type")
            } else {
                t!("crop.not_picture")
            }
        })?;
        let square = Square::new(image.width(), image.height());
        Ok(Self {
            target,
            image,
            square,
            texture: None,
            frames,
            error: None,
        })
    }

    /// The crop dialog, with a rounded-square preview matching how avatars and icons render.
    pub fn show(&mut self, ctx: &Context) -> Outcome {
        let title = match self.target {
            Target::Avatar => t!("crop.avatar"),
            Target::Icon(_) => t!("crop.icon"),
            Target::Emoji { sticker: false, .. } => t!("crop.emoji"),
            Target::Emoji { sticker: true, .. } => t!("crop.sticker"),
        };
        let mut outcome = Outcome::Open;
        let ((), dismissed) = ui::dialog(ctx, "crop", title, |ui| {
            self.editor(ui);
            ui::error_text(ui, self.error);
            ui::footer(ui, |ui| {
                if ui::primary(ui, t!("common.save")).clicked() {
                    match self.save() {
                        Ok(bytes) => outcome = Outcome::Save(bytes),
                        Err(error) => {
                            tracing::warn!(error = %format_args!("{error:#}"), "saving a crop failed");
                            self.error = Some(t!("crop.save_failed"));
                        }
                    }
                }
                if ui::secondary(ui, t!("common.cancel")).clicked() {
                    outcome = Outcome::Cancel;
                }
            });
        });
        if dismissed && matches!(outcome, Outcome::Open) {
            outcome = Outcome::Cancel;
        }
        outcome
    }

    fn editor(&mut self, ui: &mut Ui) {
        let texture = self.texture.get_or_insert_with(|| {
            let size = [self.image.width() as usize, self.image.height() as usize];
            let pixels = ColorImage::from_rgba_unmultiplied(size, self.image.as_raw());
            ui.ctx()
                .load_texture("crop", pixels, TextureOptions::LINEAR)
        });
        ui.vertical_centered(|ui| {
            let (rect, response) = ui.allocate_exact_size(Vec2::splat(VIEW), Sense::drag());
            response.widget_info(|| {
                egui::WidgetInfo::labeled(Role::Image, ui.is_enabled(), t!("crop.drag"))
            });
            if response.dragged() {
                self.square.pan(response.drag_delta(), VIEW);
            }
            if response.hovered() {
                let scroll = ui.input(|input| input.smooth_scroll_delta.y);
                if scroll != 0.0 {
                    self.square
                        .zoom_to(self.square.zoom * (scroll / 200.0).exp());
                }
            }
            if response.hovered() || response.dragged() {
                ui.ctx().set_cursor_icon(egui::CursorIcon::Grab);
            }
            let c = ui::colors(ui);
            let painter = ui.painter_at(rect);
            painter.image(texture.id(), rect, self.square.uv(), Color32::WHITE);
            // Dim what falls outside the rounded mask: a wide outside stroke covers the
            // corners and the clip keeps it in the view.
            let dim = c.main.gamma_multiply(0.85);
            let edge = Stroke::new(2.0, ui::accent(ui).base);
            let radius = match self.target {
                Target::Avatar | Target::Icon(_) => ui::avatar_radius(VIEW),
                Target::Emoji { .. } => VIEW * 0.08,
            };
            let round = CornerRadius::same(radius.round() as u8);
            painter.rect_stroke(
                rect,
                round,
                Stroke::new(VIEW * 0.3, dim),
                StrokeKind::Outside,
            );
            painter.rect_stroke(rect, round, edge, StrokeKind::Inside);
            ui.add_space(ui::SPACE_S);
            ui.horizontal(|ui| {
                ui.label(t!("crop.zoom"));
                ui.spacing_mut().slider_width = ui.available_width();
                let mut zoom = self.square.zoom;
                let slider = ui.add(
                    Slider::new(&mut zoom, 1.0..=MAX_ZOOM)
                        .logarithmic(true)
                        .show_value(false),
                );
                slider
                    .widget_info(|| egui::WidgetInfo::labeled(Role::Slider, true, t!("crop.zoom")));
                if slider.changed() {
                    self.square.zoom_to(zoom);
                }
            });
        });
    }

    /// The bytes to upload: the crop scaled and re-encoded, as a GIF when animated.
    fn save(&self) -> Result<Vec<u8>> {
        let (x, y, side) = self.square.pixels();
        if !self.frames.is_empty() {
            return encode_animated(&self.frames, (x, y, side), animated_edge(&self.target));
        }
        let crop = image::imageops::crop_imm(&self.image, x, y, side, side).to_image();
        // Server emoji and stickers are PNG or GIF only, so they never fall back to JPEG.
        match self.target {
            Target::Emoji { sticker, .. } => encode(crop, still_edge(sticker), false),
            _ => encode(crop, EDGE, true),
        }
    }
}

/// A picture file made into an emoji or sticker without asking: the centred square, as
/// the crop dialog starts out, scaled and re-encoded the same way (animation kept).
pub fn fit(bytes: &[u8], sticker: bool) -> Result<Vec<u8>> {
    let (image, frames) = decode(bytes)?;
    let (x, y, side) = Square::new(image.width(), image.height()).pixels();
    if !frames.is_empty() {
        return encode_animated(&frames, (x, y, side), emoji_animated_edge(sticker));
    }
    let crop = image::imageops::crop_imm(&image, x, y, side, side).to_image();
    encode(crop, still_edge(sticker), false)
}

/// A small preview of a picture file for lists: its first frame.
pub fn preview(bytes: &[u8]) -> Option<ColorImage> {
    let (image, _) = decode(bytes).ok()?;
    let small = DynamicImage::ImageRgba8(image)
        .thumbnail(64, 64)
        .into_rgba8();
    let size = [small.width() as usize, small.height() as usize];
    Some(ColorImage::from_rgba_unmultiplied(size, small.as_raw()))
}

/// The largest side of a still emoji or sticker.
fn still_edge(sticker: bool) -> u32 {
    if sticker { STICKER_EDGE } else { EMOJI_EDGE }
}

fn emoji_animated_edge(sticker: bool) -> u32 {
    ANIMATED_EDGE.min(still_edge(sticker))
}

/// The largest side a received emoji or sticker file may have: what apps save, so a
/// bigger one did not come from an app.
pub const fn received_edge(sticker: bool) -> u32 {
    if sticker { STICKER_EDGE } else { EMOJI_EDGE }
}

/// The largest side an animation for `target` is saved at.
fn animated_edge(target: &Target) -> u32 {
    match target {
        Target::Emoji { sticker, .. } => emoji_animated_edge(*sticker),
        _ => ANIMATED_EDGE,
    }
}

/// A picture in a format the app doesn't read, such as HEIC, AVIF, BMP or TIFF.
fn other_picture(bytes: &[u8]) -> bool {
    // HEIC/HEIF and AVIF are ISO media files: a box size, then `ftyp` and a brand.
    let heif = bytes.get(4..8) == Some(b"ftyp")
        && matches!(
            bytes.get(8..12),
            Some(
                b"heic"
                    | b"heix"
                    | b"hevc"
                    | b"hevx"
                    | b"heim"
                    | b"heis"
                    | b"mif1"
                    | b"msf1"
                    | b"avif"
                    | b"avis"
            )
        );
    heif || image::guess_format(bytes).is_ok_and(|format| {
        !matches!(
            format,
            ImageFormat::Png | ImageFormat::Jpeg | ImageFormat::Gif | ImageFormat::WebP
        )
    })
}

/// Decode a PNG, JPEG, GIF or WebP and scale it down to the working size. An animated
/// GIF or WebP also gives all its frames (scaled the same way), else none.
fn decode(bytes: &[u8]) -> Result<(RgbaImage, Vec<Frame>)> {
    let format = image::guess_format(bytes)?;
    ensure!(
        matches!(
            format,
            ImageFormat::Png | ImageFormat::Jpeg | ImageFormat::Gif | ImageFormat::WebP
        ),
        "unsupported image format {format:?}"
    );
    if matches!(format, ImageFormat::Gif | ImageFormat::WebP) {
        // A picture that is not animated, or too big to play, still works as a still.
        if let Ok(frames) = decode_frames(bytes, Some(ANIMATION_WORKING_EDGE), ANIMATION_BUDGET) {
            let first = frames[0].buffer().dimensions();
            if frames.len() > 1 && frames.iter().all(|f| f.buffer().dimensions() == first) {
                return Ok((frames[0].buffer().clone(), frames));
            }
        }
    }
    let mut reader = ImageReader::with_format(Cursor::new(bytes), format);
    let mut limits = Limits::default();
    limits.max_image_width = Some(OPEN_EDGE);
    limits.max_image_height = Some(OPEN_EDGE);
    limits.max_alloc = Some(OPEN_ALLOC);
    reader.limits(limits);
    let image = reader.decode()?;
    ensure!(image.width() > 0 && image.height() > 0, "empty image");
    let image = if image.width().max(image.height()) > WORKING_EDGE {
        image.resize(WORKING_EDGE, WORKING_EDGE, FilterType::Triangle)
    } else {
        image
    };
    Ok((image.into_rgba8(), Vec::new()))
}

/// Crop the same square out of every frame, scale it to at most `edge` and encode a
/// looping GIF that fits the size limit (also what older clients can show). Too big
/// means fewer frames (the time stays the same), then smaller frames.
fn encode_animated(frames: &[Frame], (x, y, side): (u32, u32, u32), edge: u32) -> Result<Vec<u8>> {
    let mut images: Vec<(RgbaImage, u32)> = frames
        .iter()
        .map(|frame| {
            let mut image = image::imageops::crop_imm(frame.buffer(), x, y, side, side).to_image();
            if side > edge {
                image = image::imageops::resize(&image, edge, edge, FilterType::Lanczos3);
            }
            let (numer, denom) = frame.delay().numer_denom_ms();
            (image, numer / denom.max(1))
        })
        .collect();
    loop {
        if images.len() > MAX_FRAMES {
            images = halve(images);
            continue;
        }
        let mut bytes = Vec::new();
        let mut encoder = GifEncoder::new_with_speed(&mut bytes, 10);
        encoder.set_repeat(Repeat::Infinite)?;
        encoder.encode_frames(images.iter().map(|(image, ms)| {
            Frame::from_parts(image.clone(), 0, 0, Delay::from_numer_denom_ms(*ms, 1))
        }))?;
        drop(encoder);
        if bytes.len() <= IMAGE_PLAINTEXT {
            return Ok(bytes);
        }
        if images.len() > MIN_FRAMES {
            images = halve(images);
            continue;
        }
        let width = images[0].0.width();
        ensure!(width > 16, "picture does not compress");
        let small = width * 3 / 4;
        for (image, _) in &mut images {
            *image = image::imageops::resize(image, small, small, FilterType::Lanczos3);
        }
    }
}

/// Every other frame, each one showing as long as it and the one dropped after it.
fn halve(images: Vec<(RgbaImage, u32)>) -> Vec<(RgbaImage, u32)> {
    let mut out = Vec::with_capacity(images.len() / 2 + 1);
    let mut images = images.into_iter();
    while let Some((image, ms)) = images.next() {
        let more = images.next().map_or(0, |(_, ms)| ms);
        out.push((image, ms + more));
    }
    out
}

/// Scale a square to at most `edge` pixels and encode it to fit the size limit:
/// PNG when it fits, otherwise (with `jpeg`) JPEG at falling quality, then smaller.
fn encode(mut image: RgbaImage, edge: u32, jpeg: bool) -> Result<Vec<u8>> {
    if image.width() > edge || image.height() > edge {
        image = image::imageops::resize(&image, edge, edge, FilterType::Lanczos3);
    }
    loop {
        let mut png = Cursor::new(Vec::new());
        image.write_to(&mut png, ImageFormat::Png)?;
        if png.get_ref().len() <= IMAGE_PLAINTEXT {
            return Ok(png.into_inner());
        }
        // JPEG has no transparency; large enough pictures to get here are photos.
        if !jpeg {
            ensure!(image.width() > 16, "picture does not compress");
            let side = image.width() * 3 / 4;
            image = image::imageops::resize(&image, side, side, FilterType::Lanczos3);
            continue;
        }
        let rgb = DynamicImage::ImageRgba8(image.clone()).into_rgb8();
        for quality in QUALITIES {
            let mut jpeg = Vec::new();
            JpegEncoder::new_with_quality(&mut jpeg, quality).encode_image(&rgb)?;
            if jpeg.len() <= IMAGE_PLAINTEXT {
                return Ok(jpeg);
            }
        }
        ensure!(image.width() > 16, "picture does not compress");
        let side = image.width() * 3 / 4;
        image = image::imageops::resize(&image, side, side, FilterType::Lanczos3);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusttalk_core::client::decode_profile_image;

    #[test]
    fn other_picture_formats_are_named_not_called_files() -> Result<()> {
        // A HEIC photo's first box, as Android 13's Photo Picker can hand one over.
        let heic = b"\0\0\0\x18ftypheic\0\0\0\0mif1heic";
        // The app reads no BMP; its header is enough to tell it's a picture.
        let bmp = b"BM\x46\0\0\0\0\0\0\0\x36\0\0\0";
        let mut png = Vec::new();
        DynamicImage::new_rgb8(2, 2).write_to(&mut Cursor::new(&mut png), ImageFormat::Png)?;
        assert!(other_picture(heic) && decode(heic).is_err());
        assert!(other_picture(bmp) && decode(bmp).is_err());
        assert!(!other_picture(&png) && decode(&png).is_ok());
        assert!(!other_picture(b"%PDF-1.7 not a picture"));
        assert!(!other_picture(b"\0\0\0\x18ftypisom"));
        Ok(())
    }

    #[test]
    fn zoom_and_pan_stay_inside_the_picture() {
        let mut square = Square::new(1000, 400);
        assert_eq!(square.side(), 400.0);
        square.zoom_to(0.1);
        assert_eq!(square.zoom, 1.0);
        square.zoom_to(100.0);
        assert_eq!(square.zoom, MAX_ZOOM);
        assert_eq!(square.side(), 50.0);
        square.zoom_to(1.0);
        // At zoom 1 the short side is fixed; only the long side can move.
        square.pan(Vec2::new(10_000.0, 10_000.0), VIEW);
        assert_eq!(square.center, Vec2::new(200.0, 200.0));
        assert_eq!(square.pixels(), (0, 0, 400));
        square.pan(Vec2::new(-10_000.0, -10_000.0), VIEW);
        assert_eq!(square.center, Vec2::new(800.0, 200.0));
        assert_eq!(square.pixels(), (600, 0, 400));
        // Dragging by the full view moves by one crop side.
        square.zoom_to(2.0);
        square.center = Vec2::new(500.0, 200.0);
        square.pan(Vec2::new(VIEW, 0.0), VIEW);
        assert_eq!(square.center.x, 300.0);
        // Zooming out pulls the square back inside.
        square.center = Vec2::new(900.0, 300.0);
        square.clamp();
        square.zoom_to(1.0);
        assert_eq!(square.pixels(), (600, 0, 400));
        let uv = square.uv();
        assert!(uv.min.x >= 0.0 && uv.max.x <= 1.0 && uv.min.y >= 0.0 && uv.max.y <= 1.0);
    }

    #[test]
    fn tiny_pictures_keep_a_one_pixel_crop() {
        let mut square = Square::new(1, 1);
        square.zoom_to(MAX_ZOOM);
        assert_eq!(square.pixels(), (0, 0, 1));
    }

    #[test]
    fn noisy_pictures_encode_under_the_limit() -> Result<()> {
        // Noise does not compress, so PNG is too big and JPEG has to step down.
        let mut seed = 1u32;
        let noise = RgbaImage::from_fn(1600, 1600, |_, _| {
            seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            let [a, b, c, _] = seed.to_le_bytes();
            image::Rgba([a, b, c, 255])
        });
        let png = encode(noise.clone(), STICKER_EDGE, false)?;
        assert!(png.len() <= IMAGE_PLAINTEXT);
        assert_eq!(image::guess_format(&png)?, ImageFormat::Png);
        let bytes = encode(noise, EDGE, true)?;
        assert!(bytes.len() <= IMAGE_PLAINTEXT);
        assert_eq!(image::guess_format(&bytes)?, ImageFormat::Jpeg);
        let frames = decode_profile_image(&bytes)?;
        assert!(frames[0].buffer().width() <= EDGE);
        Ok(())
    }

    #[test]
    fn plain_pictures_stay_png() -> Result<()> {
        let plain = RgbaImage::from_pixel(900, 900, image::Rgba([10, 120, 200, 128]));
        let bytes = encode(plain, EDGE, true)?;
        assert_eq!(image::guess_format(&bytes)?, ImageFormat::Png);
        assert_eq!(
            decode_profile_image(&bytes)?[0].buffer().dimensions(),
            (512, 512)
        );
        Ok(())
    }

    #[test]
    fn large_pictures_open_and_non_images_do_not() -> Result<()> {
        assert!(decode(b"not a picture").is_err());
        let mut png = Cursor::new(Vec::new());
        DynamicImage::new_rgba8(3000, 1000).write_to(&mut png, ImageFormat::Png)?;
        let (image, frames) = decode(png.get_ref())?;
        assert_eq!(image.dimensions(), (WORKING_EDGE, 683));
        assert!(frames.is_empty());
        Ok(())
    }

    fn gif(width: u32, height: u32, count: usize) -> Result<Vec<u8>> {
        let mut bytes = Vec::new();
        let mut encoder = GifEncoder::new(&mut bytes);
        encoder.encode_frames((0..count).map(|index| {
            // The left half is red, the right half changes per frame.
            let image = RgbaImage::from_fn(width, height, |x, _| {
                if x < width / 2 {
                    image::Rgba([255, 0, 0, 255])
                } else {
                    image::Rgba([0, (index * 20 % 256) as u8, 255, 255])
                }
            });
            Frame::from_parts(image, 0, 0, Delay::from_numer_denom_ms(40, 1))
        }))?;
        drop(encoder);
        Ok(bytes)
    }

    #[test]
    fn animations_crop_every_frame() -> Result<()> {
        let (image, frames) = decode(&gif(600, 300, 6)?)?;
        assert_eq!(image.dimensions(), (512, 256));
        assert_eq!(frames.len(), 6);
        // The right-hand 256x256 square: only the changing half is left of it.
        let bytes = encode_animated(&frames, (256, 0, 256), ANIMATED_EDGE)?;
        let out = decode_profile_image(&bytes)?;
        assert_eq!(out.len(), 6);
        for frame in &out {
            assert_eq!(frame.buffer().dimensions(), (256, 256));
            assert_eq!(frame.buffer().get_pixel(5, 5).0[0], 0);
        }
        let first = out[0].buffer().get_pixel(5, 5).0;
        let last = out[5].buffer().get_pixel(5, 5).0;
        assert_ne!(first, last);
        Ok(())
    }

    #[test]
    fn long_animations_keep_their_duration_within_the_limits() -> Result<()> {
        let (_, frames) = decode(&gif(64, 64, 150)?)?;
        let bytes = encode_animated(&frames, (0, 0, 64), ANIMATED_EDGE)?;
        assert!(bytes.len() <= IMAGE_PLAINTEXT);
        let out = decode_profile_image(&bytes)?;
        assert!(out.len() <= MAX_FRAMES);
        let ms: u32 = out
            .iter()
            .map(|f| f.delay().numer_denom_ms().0 / f.delay().numer_denom_ms().1)
            .sum();
        assert!((5900..=6100).contains(&ms), "{ms}");
        Ok(())
    }

    fn emoji(sticker: bool) -> Target {
        Target::Emoji { sticker, slot: 0 }
    }

    #[test]
    fn animated_emoji_and_stickers_are_capped_in_size() {
        assert_eq!(animated_edge(&emoji(false)), EMOJI_EDGE);
        assert_eq!(animated_edge(&emoji(true)), STICKER_EDGE.min(ANIMATED_EDGE));
        assert_eq!(animated_edge(&Target::Avatar), ANIMATED_EDGE);
    }

    #[test]
    fn emoji_and_stickers_keep_their_animation_when_downscaled() -> Result<()> {
        // A big animated GIF, the way it comes in through the crop dialog.
        let (_, frames) = decode(&gif(900, 900, 8)?)?;
        assert_eq!(frames.len(), 8);
        for sticker in [false, true] {
            let edge = animated_edge(&emoji(sticker));
            let bytes = encode_animated(&frames, (0, 0, 512), edge)?;
            assert_eq!(image::guess_format(&bytes)?, ImageFormat::Gif);
            // What every receiver decodes: all frames, at most the cap on a side.
            let out = decode_profile_image(&bytes)?;
            assert_eq!(out.len(), 8, "sticker: {sticker}");
            let (width, height) = out[0].buffer().dimensions();
            assert!(width.max(height) <= edge, "{width}x{height}");
            assert_ne!(
                out[0].buffer().get_pixel(width - 2, 5),
                out[7].buffer().get_pixel(width - 2, 5)
            );
        }
        Ok(())
    }

    #[test]
    fn capped_animations_stay_playable_for_receivers() -> Result<()> {
        // The most frames the largest sticker may have must pass the receiver's
        // decoded-memory check, not only the file size limit.
        let (_, frames) = decode(&gif(320, 320, 150)?)?;
        let bytes = encode_animated(&frames, (0, 0, 320), animated_edge(&emoji(true)))?;
        let out = decode_profile_image(&bytes)?;
        assert!(out.len() > 1 && out.len() <= MAX_FRAMES);
        Ok(())
    }

    #[test]
    fn webp_pictures_open() -> Result<()> {
        let mut webp = Cursor::new(Vec::new());
        RgbaImage::from_pixel(40, 30, image::Rgba([1, 2, 3, 255]))
            .write_to(&mut webp, ImageFormat::WebP)?;
        let (image, frames) = decode(webp.get_ref())?;
        assert_eq!(image.dimensions(), (40, 30));
        assert!(frames.is_empty());
        Ok(())
    }
}
