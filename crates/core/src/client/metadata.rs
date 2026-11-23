use std::io::Cursor;

use anyhow::{Context, Result, ensure};
use image::{AnimationDecoder, Frame, ImageDecoder, ImageFormat, Limits, codecs::gif::GifDecoder};
use reqwest::Method;
use serde_json::json;

use crate::keys::{IMAGE_PLAINTEXT, Metadata};

use super::{KeyClient, files::decode_limited};

const EDGE: u32 = 512;
const MAX_FRAMES: usize = 100;
const MAX_DECODED: usize = 32 * 1024 * 1024;

impl KeyClient<'_> {
    /// Validate, encrypt and replace this member's avatar.
    pub async fn set_avatar(&self, bytes: &[u8]) -> Result<()> {
        decode_profile_image(bytes)?;
        let avatar_ct = self.encrypt_metadata(
            Metadata::Avatar,
            self.identity.certificate.person_id(),
            bytes,
        )?;
        self.request(Method::PUT, "/v1/me/profile")
            .json(&json!({ "avatar_ct": avatar_ct }))
            .send()
            .await?
            .error_for_status()?;
        Ok(())
    }

    /// Fill a never-set avatar atomically. Call only when the roster advertises
    /// `avatar_initialized`: older servers ignore conditional profile fields.
    pub async fn publish_default_avatar(&self, bytes: &[u8]) -> Result<bool> {
        decode_profile_image(bytes)?;
        let avatar_ct = self.encrypt_metadata(
            Metadata::Avatar,
            self.identity.certificate.person_id(),
            bytes,
        )?;
        let reply = self
            .request(Method::PUT, "/v1/me/profile")
            .json(&json!({ "avatar_ct": avatar_ct, "avatar_if_unset": true }))
            .send()
            .await?;
        if reply.status() == reqwest::StatusCode::PRECONDITION_FAILED {
            return Ok(false);
        }
        reply.error_for_status()?;
        Ok(true)
    }

    /// Validate, encrypt and replace the server icon.
    pub async fn set_server_icon(&self, bytes: &[u8]) -> Result<()> {
        decode_profile_image(bytes)?;
        let icon_ct = self.encrypt_metadata(Metadata::ServerIcon, &self.space().id, bytes)?;
        self.request(Method::PUT, "/v1/space")
            .json(&json!({ "icon_ct": icon_ct }))
            .send()
            .await?
            .error_for_status()?;
        Ok(())
    }
}

/// Decode a PNG, JPEG or animated GIF into RGBA frames with their frame delays.
pub fn decode_profile_image(bytes: &[u8]) -> Result<Vec<Frame>> {
    ensure!(
        bytes.len() <= IMAGE_PLAINTEXT,
        "encoded image must fit in 256 KiB once encrypted"
    );
    let format = image::guess_format(bytes).context("unknown image format")?;
    if format != ImageFormat::Gif {
        let image = decode_limited(
            bytes,
            &[ImageFormat::Png, ImageFormat::Jpeg],
            EDGE,
            MAX_DECODED as u64,
        )?;
        return Ok(vec![Frame::new(image.into_rgba8())]);
    }

    let mut decoder = GifDecoder::new(Cursor::new(bytes))?;
    let mut limits = Limits::default();
    limits.max_image_width = Some(EDGE);
    limits.max_image_height = Some(EDGE);
    limits.max_alloc = Some(MAX_DECODED as u64);
    decoder.set_limits(limits)?;
    let (width, height) = decoder.dimensions();
    ensure!(width > 0 && height > 0, "GIF has an empty canvas");
    let limit = MAX_FRAMES.min(MAX_DECODED / (4 * width as usize * height as usize));
    let mut frames = Vec::new();
    for frame in decoder.into_frames() {
        ensure!(
            frames.len() < limit,
            "GIF frame or decoded memory limit exceeded"
        );
        frames.push(frame?);
    }
    ensure!(!frames.is_empty(), "GIF has no frames");
    Ok(frames)
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::{Delay, DynamicImage, Rgba, RgbaImage, codecs::gif::GifEncoder};

    fn gif(width: u32, height: u32, frames: usize) -> Result<Vec<u8>> {
        let mut bytes = Vec::new();
        let mut encoder = GifEncoder::new(&mut bytes);
        encoder.encode_frames((0..frames).map(|index| {
            Frame::from_parts(
                RgbaImage::from_pixel(width, height, Rgba([index as u8, 30, 60, 255])),
                0,
                0,
                Delay::from_numer_denom_ms(40, 1),
            )
        }))?;
        drop(encoder);
        Ok(bytes)
    }

    #[test]
    fn rejects_hostile_images() -> Result<()> {
        let mut png = Cursor::new(Vec::new());
        DynamicImage::new_rgba8(EDGE + 1, 1).write_to(&mut png, ImageFormat::Png)?;
        assert!(decode_profile_image(png.get_ref()).is_err());
        assert!(decode_profile_image(&gif(1, EDGE + 1, 1)?).is_err());
        assert!(decode_profile_image(&gif(1, 1, MAX_FRAMES + 1)?).is_err());
        assert!(decode_profile_image(&gif(EDGE, EDGE, 33)?).is_err());
        assert!(decode_profile_image(b"not an image").is_err());
        let mut oversized = gif(1, 1, 1)?;
        oversized.resize(IMAGE_PLAINTEXT + 1, 0);
        assert!(decode_profile_image(&oversized).is_err());
        Ok(())
    }

    #[test]
    fn decodes_animated_gif_frames_and_delays() -> Result<()> {
        let frames = decode_profile_image(&gif(2, 3, 2)?)?;
        assert_eq!(frames.len(), 2);
        assert_eq!(frames[0].buffer().dimensions(), (2, 3));
        assert_eq!(frames[1].buffer().get_pixel(0, 0).0, [1, 30, 60, 255]);
        assert_eq!(frames[1].delay(), Delay::from_numer_denom_ms(40, 1));
        Ok(())
    }
}
