//! The crop step after picking a profile picture or server icon: move and zoom a
//! square over the picture, then save it scaled and re-encoded to fit the limits.

use std::io::Cursor;

use anyhow::{Result, ensure};
use eframe::egui::{
    self, Color32, ColorImage, Context, CornerRadius, Rect, Role, Sense, Slider, Stroke,
    StrokeKind, TextureHandle, TextureOptions, Ui, Vec2, pos2,
};
use image::{
    DynamicImage, ImageFormat, ImageReader, Limits, RgbaImage, codecs::jpeg::JpegEncoder,
    imageops::FilterType,
};
use rusttalk_core::{client::decode_profile_image, keys::IMAGE_PLAINTEXT};

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

/// Server emoji and stickers are saved at most this large: plenty for how they show.
const EMOJI_EDGE: u32 = 128;
const STICKER_EDGE: u32 = 320;

/// What the picture is for.
#[derive(Clone)]
pub enum Target {
    Avatar,
    Icon(url::Url),
    /// A new server emoji or (`sticker`) sticker with this name.
    Emoji {
        address: url::Url,
        name: String,
        sticker: bool,
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
    /// An animated GIF that already fits, which can be kept as it is.
    animated: Option<Vec<u8>>,
    /// An animated GIF that doesn't fit: cropping it keeps only the first frame.
    stills: bool,
    keep_animation: bool,
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
        let (image, animated_gif) = decode(&bytes).map_err(|error| {
            tracing::info!(error = %format_args!("{error:#}"), "picture refused");
            t!("crop.not_picture")
        })?;
        let fits = animated_gif && decode_profile_image(&bytes).is_ok();
        let square = Square::new(image.width(), image.height());
        Ok(Self {
            target,
            image,
            square,
            texture: None,
            animated: fits.then_some(bytes),
            stills: animated_gif && !fits,
            keep_animation: fits,
            error: None,
        })
    }

    /// The crop dialog, with a round preview for profile pictures and a rounded
    /// square for server icons.
    pub fn show(&mut self, ctx: &Context) -> Outcome {
        let title = match self.target {
            Target::Avatar => t!("crop.avatar"),
            Target::Icon(_) => t!("crop.icon"),
            Target::Emoji { sticker: false, .. } => t!("crop.emoji"),
            Target::Emoji { sticker: true, .. } => t!("crop.sticker"),
        };
        let mut outcome = Outcome::Open;
        let ((), dismissed) = ui::dialog(ctx, "crop", title, |ui| {
            let still = !self.keep_animation;
            ui.add_enabled_ui(still, |ui| self.editor(ui));
            if self.animated.is_some() {
                ui::checkbox(ui, &mut self.keep_animation, t!("crop.keep_gif"));
            } else if self.stills {
                ui.label(ui::caption(t!("crop.gif_still")));
            }
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
            if ui.is_enabled() {
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
            }
            let c = ui::colors(ui);
            let painter = ui.painter_at(rect);
            painter.image(texture.id(), rect, self.square.uv(), Color32::WHITE);
            // Dim what falls outside the round or rounded mask: a wide outside
            // stroke covers the corners and the clip keeps it in the view.
            let dim = c.main.gamma_multiply(0.85);
            let edge = Stroke::new(2.0, ui::accent(ui).base);
            match self.target {
                Target::Avatar => {
                    let r = VIEW / 2.0;
                    let wide = r * (std::f32::consts::SQRT_2 - 1.0) + 2.0;
                    painter.circle_stroke(rect.center(), r + wide / 2.0, Stroke::new(wide, dim));
                    painter.circle_stroke(rect.center(), r - 1.0, edge);
                }
                Target::Icon(_) | Target::Emoji { .. } => {
                    let corner = if matches!(self.target, Target::Icon(_)) {
                        0.25
                    } else {
                        0.08
                    };
                    let round = CornerRadius::same((VIEW * corner) as u8);
                    painter.rect_stroke(
                        rect,
                        round,
                        Stroke::new(VIEW * 0.3, dim),
                        StrokeKind::Outside,
                    );
                    painter.rect_stroke(rect, round, edge, StrokeKind::Inside);
                }
            }
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

    /// The bytes to upload: the GIF as it is, or the crop scaled and re-encoded.
    fn save(&self) -> Result<Vec<u8>> {
        if let (true, Some(bytes)) = (self.keep_animation, &self.animated) {
            return Ok(bytes.clone());
        }
        let (x, y, side) = self.square.pixels();
        let crop = image::imageops::crop_imm(&self.image, x, y, side, side).to_image();
        // Server emoji and stickers are PNG or GIF only, so they never fall back to JPEG.
        match self.target {
            Target::Emoji { sticker, .. } => {
                encode(crop, if sticker { STICKER_EDGE } else { EMOJI_EDGE }, false)
            }
            _ => encode(crop, EDGE, true),
        }
    }
}

/// Decode a PNG, JPEG or GIF (its first frame) and scale it down to the working
/// size. Also says whether it is a GIF with more than one frame.
fn decode(bytes: &[u8]) -> Result<(RgbaImage, bool)> {
    let format = image::guess_format(bytes)?;
    ensure!(
        matches!(
            format,
            ImageFormat::Png | ImageFormat::Jpeg | ImageFormat::Gif
        ),
        "unsupported image format {format:?}"
    );
    let mut reader = ImageReader::with_format(Cursor::new(bytes), format);
    let mut limits = Limits::default();
    limits.max_image_width = Some(OPEN_EDGE);
    limits.max_image_height = Some(OPEN_EDGE);
    limits.max_alloc = Some(OPEN_ALLOC);
    reader.limits(limits);
    let image = reader.decode()?;
    ensure!(image.width() > 0 && image.height() > 0, "empty image");
    let animated = format == ImageFormat::Gif && gif_frames(bytes) > 1;
    let image = if image.width().max(image.height()) > WORKING_EDGE {
        image.resize(WORKING_EDGE, WORKING_EDGE, FilterType::Triangle)
    } else {
        image
    };
    Ok((image.into_rgba8(), animated))
}

/// Frames in a GIF, counting no further than two.
fn gif_frames(bytes: &[u8]) -> usize {
    use image::AnimationDecoder;
    image::codecs::gif::GifDecoder::new(Cursor::new(bytes))
        .map(|decoder| decoder.into_frames().take(2).count())
        .unwrap_or(0)
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
        let (image, animated) = decode(png.get_ref())?;
        assert_eq!(image.dimensions(), (WORKING_EDGE, 683));
        assert!(!animated);
        Ok(())
    }
}
