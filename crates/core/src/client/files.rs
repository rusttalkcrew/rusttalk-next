use std::{
    io::{self, Cursor},
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

use anyhow::{Context, Result, ensure};
use chacha20poly1305::{
    KeyInit, XChaCha20Poly1305, XNonce,
    aead::{Aead, Payload},
};
use futures_util::stream;
use image::{
    AnimationDecoder, DynamicImage, Frame, ImageDecoder, ImageFormat, ImageReader, Limits,
    codecs::{gif::GifDecoder, webp::WebPDecoder},
    metadata::Orientation,
};
use reqwest::{Body, Method, StatusCode, header::CONTENT_TYPE};
use serde::Deserialize;
use tokio::time;
use zeroize::Zeroizing;

use crate::{
    auth::{decode, encode},
    keys::random_key,
    messages::{Attachment, BlobRef, CHUNK_BYTES, MAX_CIPHERTEXT, MAX_THUMBNAIL_BYTES, TAG_BYTES},
};

use super::{KeyClient, Server};

/// A transfer fails when no bytes move for this long, so slow links still finish.
const TRANSFER_IDLE: Duration = Duration::from_secs(30);
/// The longest one file transfer may take.
const TRANSFER_LIMIT: Duration = Duration::from_secs(15 * 60);
/// Upload body piece size; each piece taken by the connection counts as progress.
const UPLOAD_PIECE: usize = 64 * 1024;
/// How often, and how long at most, an upload waits when the server asks it to slow
/// down. Uploads run beside the event loop, so waiting holds up nothing else.
const SLOW_DOWN_TRIES: u32 = 5;
const SLOW_DOWN_WAIT: Duration = Duration::from_secs(15);

/// Called with the bytes moved so far and the total, as a transfer goes.
pub type Progress = Arc<dyn Fn(u64, u64) + Send + Sync>;

/// The API refused an upload. `code` is its error code, such as `file_member_quota`.
#[derive(Debug, thiserror::Error)]
#[error("upload rejected with {status}: {code}")]
pub struct UploadRejected {
    pub status: StatusCode,
    pub code: String,
}
const THUMBNAIL_EDGE: u32 = 320;

/// The server and session a file transfer needs, owned, so a transfer can run in a task
/// of its own while the event loop goes on.
#[derive(Clone)]
pub struct FileClient {
    server: Server,
    token: String,
}

impl FileClient {
    pub(super) fn new(server: Server, token: String) -> Self {
        Self { server, token }
    }

    fn request(&self, method: Method, path: &str) -> reqwest::RequestBuilder {
        self.server.request(method, path).bearer_auth(&self.token)
    }

    /// Encrypt and upload a file and, for PNG/JPEG, an independently encrypted PNG thumbnail.
    /// `progress` follows the file itself, not its small thumbnail.
    pub async fn upload_file(
        &self,
        channel: &[u8; 32],
        name: &str,
        media_type: &str,
        bytes: &[u8],
        progress: Option<Progress>,
    ) -> Result<Attachment> {
        let (attachment, thumbnail) = prepare_file(name, media_type, bytes, bytes)?;
        self.upload_prepared(channel, &attachment, bytes, thumbnail.as_deref(), progress)
            .await?;
        Ok(attachment)
    }

    /// Upload a file made by `prepare_file`. On failure the file blob is removed again; a
    /// caller that gives up midway removes both blobs by the references it already has.
    pub async fn upload_prepared(
        &self,
        channel: &[u8; 32],
        attachment: &Attachment,
        bytes: &[u8],
        thumbnail: Option<&[u8]>,
        progress: Option<Progress>,
    ) -> Result<()> {
        self.upload_blob(channel, &attachment.file, bytes, progress)
            .await?;
        if let (Some(reference), Some(bytes)) = (&attachment.thumbnail, thumbnail)
            && let Err(error) = self.upload_blob(channel, reference, bytes, None).await
        {
            if let Err(cleanup) = self.delete_file(&attachment.file.id).await {
                tracing::warn!(status = ?cleanup.downcast_ref::<reqwest::Error>().and_then(reqwest::Error::status),
                    "could not remove file after thumbnail upload failed");
            }
            return Err(error);
        }
        Ok(())
    }

    async fn upload_blob(
        &self,
        channel: &[u8; 32],
        reference: &BlobRef,
        bytes: &[u8],
        progress: Option<Progress>,
    ) -> Result<()> {
        #[derive(Deserialize)]
        struct Uploaded {
            id: String,
        }
        let ciphertext: Arc<[u8]> = seal(reference, bytes)?.into();
        #[derive(Deserialize)]
        struct Problem {
            error: String,
        }
        let mut tries = 0;
        let response = loop {
            let response = self
                .send_blob(channel, reference, &ciphertext, &progress)
                .await?;
            // Asked to slow down: wait as asked, a few times, then fail as before.
            match super::RateLimited::from_response(&response) {
                Some(super::RateLimited(wait))
                    if tries < SLOW_DOWN_TRIES && wait <= SLOW_DOWN_WAIT =>
                {
                    tries += 1;
                    tracing::info!(?wait, "upload rate limited; trying again");
                    time::sleep(wait).await;
                }
                _ => break response,
            }
        };
        let status = response.status();
        if !status.is_success() {
            let code = response.json::<Problem>().await.map(|p| p.error);
            return Err(UploadRejected {
                status,
                code: code.unwrap_or_default(),
            }
            .into());
        }
        let uploaded: Uploaded = response.json().await?;
        ensure!(
            decode::<32>(&uploaded.id)? == reference.id,
            "wrong uploaded file ID"
        );
        Ok(())
    }

    async fn send_blob(
        &self,
        channel: &[u8; 32],
        reference: &BlobRef,
        ciphertext: &Arc<[u8]>,
        progress: &Option<Progress>,
    ) -> Result<reqwest::Response> {
        let taken = Arc::new(AtomicUsize::new(0));
        let pieces = {
            let (taken, ciphertext, progress) =
                (taken.clone(), ciphertext.clone(), progress.clone());
            let ends = (UPLOAD_PIECE..ciphertext.len() + UPLOAD_PIECE).step_by(UPLOAD_PIECE);
            let total = ciphertext.len() as u64;
            stream::iter(ends.map(move |end| {
                let start = end - UPLOAD_PIECE;
                taken.fetch_add(1, Ordering::Relaxed);
                if let Some(progress) = &progress {
                    progress((end as u64).min(total), total);
                }
                Ok::<_, io::Error>(ciphertext[start..end.min(ciphertext.len())].to_vec())
            }))
        };
        let send = self
            .request(Method::POST, "/v1/files")
            .query(&[("channel", encode(channel)), ("id", encode(&reference.id))])
            .header(CONTENT_TYPE, "application/octet-stream")
            .timeout(TRANSFER_LIMIT)
            .body(Body::wrap_stream(pieces))
            .send();
        let mut send = std::pin::pin!(send);
        loop {
            let before = taken.load(Ordering::Relaxed);
            match time::timeout(TRANSFER_IDLE, &mut send).await {
                Ok(response) => return Ok(response?),
                Err(_) => ensure!(taken.load(Ordering::Relaxed) != before, "upload stalled"),
            }
        }
    }

    /// Download a file or thumbnail and return bytes only after authenticating the entire blob.
    pub async fn download_file(
        &self,
        reference: &BlobRef,
        progress: Option<Progress>,
    ) -> Result<Vec<u8>> {
        let expected = reference.ciphertext_len()?;
        let request = self
            .request(Method::GET, &file_path(&reference.id))
            .timeout(TRANSFER_LIMIT)
            .send();
        let mut response = time::timeout(TRANSFER_IDLE, request)
            .await
            .context("download stalled")??
            .error_for_status()?;
        let mut ciphertext = Vec::new();
        while let Some(chunk) = time::timeout(TRANSFER_IDLE, response.chunk())
            .await
            .context("download stalled")??
        {
            ensure!(
                chunk.len() <= expected - ciphertext.len(),
                "oversized ciphertext"
            );
            ciphertext.extend_from_slice(&chunk);
            if let Some(progress) = &progress {
                progress(ciphertext.len() as u64, expected as u64);
            }
        }
        open(reference, &ciphertext)
    }

    /// Remove an uploaded file or thumbnail by its ID.
    pub async fn delete_file(&self, id: &[u8; 32]) -> Result<()> {
        self.request(Method::DELETE, &file_path(id))
            .send()
            .await?
            .error_for_status()?;
        Ok(())
    }
}

/// The same transfers on the event loop's own client.
impl KeyClient<'_> {
    pub async fn upload_file(
        &self,
        channel: &[u8; 32],
        name: &str,
        media_type: &str,
        bytes: &[u8],
        progress: Option<Progress>,
    ) -> Result<Attachment> {
        (self.file_client())
            .upload_file(channel, name, media_type, bytes, progress)
            .await
    }

    pub async fn upload_prepared(
        &self,
        channel: &[u8; 32],
        attachment: &Attachment,
        bytes: &[u8],
        thumbnail: Option<&[u8]>,
        progress: Option<Progress>,
    ) -> Result<()> {
        (self.file_client())
            .upload_prepared(channel, attachment, bytes, thumbnail, progress)
            .await
    }

    pub async fn download_file(
        &self,
        reference: &BlobRef,
        progress: Option<Progress>,
    ) -> Result<Vec<u8>> {
        self.file_client().download_file(reference, progress).await
    }

    pub async fn delete_file(&self, id: &[u8; 32]) -> Result<()> {
        self.file_client().delete_file(id).await
    }

    /// Remove a file attached to a deleted message. `uploader` is the message's sender; a
    /// moderator's request fails with 404 when the file belongs to anyone else.
    pub async fn delete_message_file(&self, id: &[u8; 32], uploader: &[u8; 32]) -> Result<()> {
        self.request(Method::DELETE, &file_path(id))
            .query(&[("uploader", encode(uploader))])
            .send()
            .await?
            .error_for_status()?;
        Ok(())
    }
}

const PICTURE_FORMATS: &[ImageFormat] = &[
    ImageFormat::Png,
    ImageFormat::Jpeg,
    ImageFormat::Gif,
    ImageFormat::WebP,
];
/// The most pixels per edge of any picture, so panoramas still load.
const PICTURE_EDGE: u32 = 16_384;
/// The most decoded bytes one picture or frame may take, after JPEG scaling.
const PICTURE_ALLOC: u64 = 64 * 1024 * 1024;
/// The most pixels a JPEG may have before scaling: 8192×8192, more than a 48 MP photo.
const JPEG_PIXELS: u64 = 8192 * 8192;
/// A progressive JPEG keeps all its DCT coefficients (2 bytes each) at full size while
/// decoding; this bounds them, which still fits a 24 MP photo.
const PROGRESSIVE_ALLOC: u64 = 192 * 1024 * 1024;
/// Stills shown in full shrink to this many pixels per edge, about a 4K screen.
const VIEW_EDGE: u32 = 4096;
/// Frames of one animation, however small they are.
const MAX_ANIMATION_FRAMES: usize = 400;

/// Decode a PNG, JPEG, GIF or WebP (its first frame), turned upright by its EXIF
/// orientation and at most `VIEW_EDGE` pixels per edge, with dimension and allocation
/// limits for hostile input.
pub fn decode_image(bytes: &[u8]) -> Result<DynamicImage> {
    decode_still(bytes, None)
}

/// A still picture fit to `fit` pixels per edge, or shrunk to `VIEW_EDGE` without one.
/// A JPEG decodes at the smallest DCT scale that still fills that size, so a 48 MP
/// photo never needs all its pixels in memory.
fn decode_still(bytes: &[u8], fit: Option<u32>) -> Result<DynamicImage> {
    let jpeg = image::guess_format(bytes).ok() == Some(ImageFormat::Jpeg);
    let scaled = jpeg.then(|| decode_jpeg(bytes, fit.unwrap_or(VIEW_EDGE / 2)));
    let image = match scaled {
        Some(Ok(image)) => image,
        // Kinds the scaling decoder lacks (CMYK, 12-bit, arithmetic) get the general
        // decoder and its usual limits.
        _ => decode_limited(bytes, PICTURE_FORMATS, PICTURE_EDGE, PICTURE_ALLOC)?,
    };
    Ok(match fit {
        Some(fit) => image.thumbnail(fit, fit),
        None if image.width().max(image.height()) > VIEW_EDGE => {
            image.thumbnail(VIEW_EDGE, VIEW_EDGE)
        }
        None => image,
    })
}

/// Decode a JPEG scaled down by 1/2, 1/4 or 1/8 while still at least `edge` pixels on
/// its longer side, and turn it upright.
fn decode_jpeg(bytes: &[u8], edge: u32) -> Result<DynamicImage> {
    use jpeg_decoder::{CodingProcess, Decoder, PixelFormat};
    ensure!(bytes.len() <= MAX_CIPHERTEXT, "image input too large");
    let mut decoder = Decoder::new(bytes);
    decoder.read_info()?;
    let info = decoder.info().context("JPEG without a header")?;
    let (width, height) = (u32::from(info.width), u32::from(info.height));
    let pixels = u64::from(width) * u64::from(height);
    ensure!(
        width.max(height) <= PICTURE_EDGE && pixels <= JPEG_PIXELS,
        "picture too large"
    );
    ensure!(
        info.coding_process != CodingProcess::DctProgressive
            || pixels * info.pixel_format.pixel_bytes() as u64 * 2 <= PROGRESSIVE_ALLOC,
        "progressive picture too large"
    );
    let edge = edge.min(u32::from(u16::MAX)) as u16;
    let (width, height) = decoder.scale(edge, edge)?;
    decoder.set_max_decoding_buffer_size(PICTURE_ALLOC as usize);
    let pixels = decoder.decode()?;
    let (width, height) = (u32::from(width), u32::from(height));
    let mut image = match decoder.info().map(|info| info.pixel_format) {
        Some(PixelFormat::RGB24) => {
            image::RgbImage::from_raw(width, height, pixels).map(DynamicImage::ImageRgb8)
        }
        Some(PixelFormat::L8) => {
            image::GrayImage::from_raw(width, height, pixels).map(DynamicImage::ImageLuma8)
        }
        _ => None,
    }
    .context("unsupported JPEG pixel format")?;
    if let Some(orientation) = decoder.exif_data().and_then(Orientation::from_exif_chunk) {
        image.apply_orientation(orientation);
    }
    Ok(image)
}

/// One frame, for pictures that are not animated.
fn still(bytes: &[u8], fit: Option<u32>) -> Result<Vec<Frame>> {
    Ok(vec![Frame::new(decode_still(bytes, fit)?.into_rgba8())])
}

/// Decode every frame of a picture with its delay, for animated GIF and WebP (other
/// formats give one frame). Frames larger than `fit` pixels per edge shrink to it, and
/// the decoded frames together stay within `budget` bytes, else this fails: received
/// pictures are hostile.
pub fn decode_frames(bytes: &[u8], fit: Option<u32>, budget: usize) -> Result<Vec<Frame>> {
    decode_frames_until(bytes, fit, budget, || true)
}

/// `decode_frames` that stops with an error once `keep_going` turns false, checked
/// before each frame, so work nobody waits for any more ends early.
pub fn decode_frames_until(
    bytes: &[u8],
    fit: Option<u32>,
    budget: usize,
    keep_going: impl Fn() -> bool,
) -> Result<Vec<Frame>> {
    ensure!(bytes.len() <= MAX_CIPHERTEXT, "image input too large");
    let format = image::guess_format(bytes).context("unknown image format")?;
    ensure!(
        PICTURE_FORMATS.contains(&format),
        "unsupported image format"
    );
    let mut limits = Limits::default();
    limits.max_image_width = Some(PICTURE_EDGE);
    limits.max_image_height = Some(PICTURE_EDGE);
    limits.max_alloc = Some(PICTURE_ALLOC);
    let frames = match format {
        ImageFormat::Gif => {
            let mut decoder = GifDecoder::new(Cursor::new(bytes))?;
            decoder.set_limits(limits)?;
            decoder.into_frames()
        }
        ImageFormat::WebP => {
            let mut decoder = WebPDecoder::new(Cursor::new(bytes))?;
            decoder.set_limits(limits)?;
            // A still WebP has no animation frames to iterate.
            if decoder.has_animation() {
                decoder.into_frames()
            } else {
                return still(bytes, fit);
            }
        }
        _ => return still(bytes, fit),
    };
    let (mut out, mut used) = (Vec::new(), 0usize);
    for frame in frames {
        ensure!(keep_going(), "decoding cancelled");
        ensure!(out.len() < MAX_ANIMATION_FRAMES, "too many frames");
        let mut frame = frame?;
        if let Some(fit) = fit {
            let (delay, left, top) = (frame.delay(), frame.left(), frame.top());
            let small = DynamicImage::ImageRgba8(frame.into_buffer()).thumbnail(fit, fit);
            frame = Frame::from_parts(small.into_rgba8(), left, top, delay);
        }
        used += frame.buffer().as_raw().len();
        ensure!(used <= budget, "picture too big to play");
        out.push(frame);
    }
    ensure!(!out.is_empty(), "picture has no frames");
    Ok(out)
}

/// Decode a received thumbnail: only the PNG this client makes, at most 320 pixels per edge.
pub fn decode_thumbnail(bytes: &[u8]) -> Result<DynamicImage> {
    ensure!(bytes.len() <= MAX_THUMBNAIL_BYTES, "thumbnail too large");
    let edge = THUMBNAIL_EDGE as u64;
    decode_limited(bytes, &[ImageFormat::Png], THUMBNAIL_EDGE, edge * edge * 8)
}

pub(super) fn decode_limited(
    bytes: &[u8],
    formats: &[ImageFormat],
    edge: u32,
    alloc: u64,
) -> Result<DynamicImage> {
    ensure!(bytes.len() <= MAX_CIPHERTEXT, "image input too large");
    let format = image::guess_format(bytes).context("unknown image format")?;
    ensure!(formats.contains(&format), "unsupported image format");
    let mut reader = ImageReader::with_format(Cursor::new(bytes), format);
    let mut limits = Limits::default();
    limits.max_image_width = Some(edge);
    limits.max_image_height = Some(edge);
    limits.max_alloc = Some(alloc);
    reader.limits(limits.clone());
    let mut decoder = reader.into_decoder()?;
    limits.reserve(decoder.total_bytes())?;
    let orientation = decoder.orientation()?;
    let mut image = DynamicImage::from_decoder(decoder)?;
    image.apply_orientation(orientation);
    Ok(image)
}

/// A PNG thumbnail (of the first frame) for a decodable PNG, JPEG, GIF or WebP. Anything else, including images
/// over the decode limits, is still sent, just without a thumbnail.
fn make_thumbnail(bytes: &[u8]) -> Option<Vec<u8>> {
    let image = decode_still(bytes, Some(THUMBNAIL_EDGE)).ok()?;
    let mut png = Cursor::new(Vec::new());
    DynamicImage::ImageRgba8(image.into_rgba8())
        .write_to(&mut png, ImageFormat::Png)
        .ok()?;
    let png = png.into_inner();
    (png.len() <= MAX_THUMBNAIL_BYTES).then_some(png)
}

/// A new file's references and its PNG thumbnail (made from `picture`, which is the
/// file itself for pictures, or a link preview's page picture), before any upload.
pub fn prepare_file(
    name: &str,
    media_type: &str,
    bytes: &[u8],
    picture: &[u8],
) -> Result<(Attachment, Option<Vec<u8>>)> {
    let mut attachment = Attachment {
        file: blob_ref(bytes.len())?,
        name: name.into(),
        media_type: media_type.into(),
        thumbnail: None,
    };
    attachment.validate()?;
    let thumbnail = make_thumbnail(picture);
    if let Some(png) = &thumbnail {
        attachment.thumbnail = Some(blob_ref(png.len())?);
    }
    Ok((attachment, thumbnail))
}

fn file_path(id: &[u8; 32]) -> String {
    format!("/v1/files/{}", encode(id))
}

fn blob_ref(len: usize) -> Result<BlobRef> {
    let reference = BlobRef {
        id: *random_key()?,
        key: random_key()?,
        size: len.try_into()?,
        chunk_size: CHUNK_BYTES,
    };
    reference.ciphertext_len()?;
    Ok(reference)
}

fn chunk_context(id: &[u8; 32], index: usize, final_chunk: bool) -> (XNonce, Vec<u8>) {
    let index = (index as u64).to_be_bytes();
    let mut nonce = [0; 24];
    nonce[16..].copy_from_slice(&index);
    let ad = [
        b"rusttalk-file-chunk-v1".as_slice(),
        id,
        &index,
        &[u8::from(final_chunk)],
    ]
    .concat();
    (nonce.into(), ad)
}

fn seal(reference: &BlobRef, bytes: &[u8]) -> Result<Vec<u8>> {
    let mut out = Vec::with_capacity(reference.ciphertext_len()?);
    ensure!(
        bytes.len() == reference.size as usize,
        "wrong plaintext length"
    );
    let cipher = XChaCha20Poly1305::new(&(*reference.key).into());
    let chunks = bytes.len().div_ceil(reference.chunk_size as usize).max(1);
    for index in 0..chunks {
        let start = index * reference.chunk_size as usize;
        let end = (start + reference.chunk_size as usize).min(bytes.len());
        let (nonce, aad) = chunk_context(&reference.id, index, index + 1 == chunks);
        let sealed = cipher
            .encrypt(
                &nonce,
                Payload {
                    msg: &bytes[start..end],
                    aad: &aad,
                },
            )
            .map_err(|_| anyhow::anyhow!("file chunk encryption failed"))?;
        out.extend_from_slice(&sealed);
    }
    Ok(out)
}

fn open(reference: &BlobRef, ciphertext: &[u8]) -> Result<Vec<u8>> {
    ensure!(
        ciphertext.len() == reference.ciphertext_len()?,
        "wrong ciphertext length"
    );
    let cipher = XChaCha20Poly1305::new(&(*reference.key).into());
    let mut out = Zeroizing::new(Vec::with_capacity(reference.size as usize));
    let chunks = ciphertext.chunks(reference.chunk_size as usize + TAG_BYTES);
    let count = chunks.len();
    for (index, chunk) in chunks.enumerate() {
        let (nonce, aad) = chunk_context(&reference.id, index, index + 1 == count);
        let plain = Zeroizing::new(
            cipher
                .decrypt(
                    &nonce,
                    Payload {
                        msg: chunk,
                        aad: &aad,
                    },
                )
                .map_err(|_| anyhow::anyhow!("file chunk authentication failed"))?,
        );
        out.extend_from_slice(&plain);
    }
    Ok(std::mem::take(&mut *out))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chunks_bind_order_final_flag_and_file_id() -> Result<()> {
        let plaintext = [
            vec![1; CHUNK_BYTES as usize],
            vec![2; CHUNK_BYTES as usize],
            vec![3],
        ]
        .concat();
        let reference = blob_ref(plaintext.len())?;
        let ciphertext = seal(&reference, &plaintext)?;
        assert_eq!(open(&reference, &ciphertext)?, plaintext);

        let mut altered = ciphertext.clone();
        altered[0] ^= 1;
        assert!(open(&reference, &altered).is_err());
        let stride = CHUNK_BYTES as usize + TAG_BYTES;
        let reordered = [
            &ciphertext[stride..2 * stride],
            &ciphertext[..stride],
            &ciphertext[2 * stride..],
        ]
        .concat();
        assert!(open(&reference, &reordered).is_err());
        assert!(open(&reference, &ciphertext[..ciphertext.len() - 1]).is_err());
        let mut changed = reference;
        changed.size = CHUNK_BYTES * 2;
        assert!(open(&changed, &ciphertext[..2 * stride]).is_err());
        changed.size = plaintext.len() as u32;
        changed.id[0] ^= 1;
        assert!(open(&changed, &ciphertext).is_err());

        let empty = blob_ref(0)?;
        let empty_blob = seal(&empty, b"")?;
        assert_eq!(empty_blob.len(), TAG_BYTES);
        assert!(open(&empty, &empty_blob)?.is_empty());
        Ok(())
    }

    #[test]
    fn image_decoder_rejects_hostile_dimensions_and_non_images() -> Result<()> {
        let mut png = Cursor::new(Vec::new());
        DynamicImage::new_rgb8(PICTURE_EDGE + 1, 1).write_to(&mut png, ImageFormat::Png)?;
        let error = decode_image(png.get_ref())
            .err()
            .unwrap()
            .downcast::<image::ImageError>()?;
        assert!(matches!(error, image::ImageError::Limits(limit)
            if limit.kind() == image::error::LimitErrorKind::DimensionError));
        assert!(decode_image(b"not an image at all").is_err());
        // The 4096×2049 RGBA16 header exceeds the allocation budget before pixel data is read.
        let oversized = b"\x89\x50\x4e\x47\x0d\x0a\x1a\x0a\x00\x00\x00\x0d\x49\x48\x44\x52\x00\x00\x10\x00\x00\x00\x08\x01\x10\x06\x00\x00\x00\x3d\x5e\xf9\x58\x00\x00\x00\x09\x49\x44\x41\x54\x78\x9c\x63\x00\x00\x00\x01\x00\x01\x5e\xff\x7d\xf9\x00\x00\x00\x00\x49\x45\x4e\x44\xae\x42\x60\x82";
        let error = decode_image(oversized)
            .err()
            .unwrap()
            .downcast::<image::ImageError>()?;
        assert!(matches!(error, image::ImageError::Limits(_)));
        Ok(())
    }

    fn png(width: u32, height: u32) -> Result<Vec<u8>> {
        let mut png = Cursor::new(Vec::new());
        DynamicImage::new_rgb8(width, height).write_to(&mut png, ImageFormat::Png)?;
        Ok(png.into_inner())
    }

    fn gif(width: u32, height: u32, frames: usize) -> Result<Vec<u8>> {
        let mut bytes = Vec::new();
        let mut encoder = image::codecs::gif::GifEncoder::new(&mut bytes);
        encoder.encode_frames((0..frames).map(|at| {
            let color = image::Rgba([(at * 60) as u8, 0, 200, 255]);
            Frame::from_parts(
                image::RgbaImage::from_pixel(width, height, color),
                0,
                0,
                image::Delay::from_numer_denom_ms(100, 1),
            )
        }))?;
        drop(encoder);
        Ok(bytes)
    }

    fn webp(width: u32, height: u32) -> Result<Vec<u8>> {
        let mut bytes = Vec::new();
        DynamicImage::new_rgba8(width, height)
            .write_to(&mut Cursor::new(&mut bytes), ImageFormat::WebP)?;
        Ok(bytes)
    }

    #[test]
    fn gif_and_webp_get_a_first_frame_thumbnail() -> Result<()> {
        for bytes in [gif(640, 240, 1)?, gif(640, 240, 3)?, webp(640, 240)?] {
            let thumbnail = make_thumbnail(&bytes).context("missing thumbnail")?;
            let decoded = decode_thumbnail(&thumbnail)?;
            assert_eq!((decoded.width(), decoded.height()), (320, 120));
        }
        // Over the decode limits: still sent, without a thumbnail.
        assert!(make_thumbnail(&gif(PICTURE_EDGE + 1, 1, 1)?).is_none());
        Ok(())
    }

    #[test]
    fn animations_decode_with_limits() -> Result<()> {
        let still = decode_frames(&gif(64, 64, 1)?, None, 1 << 20)?;
        assert_eq!(still.len(), 1);
        let frames = decode_frames(&gif(640, 240, 3)?, Some(320), 1 << 20)?;
        assert_eq!(frames.len(), 3);
        assert_eq!(frames[0].buffer().dimensions(), (320, 120));
        assert_eq!(decode_frames(&webp(32, 32)?, None, 1 << 20)?.len(), 1);
        // The memory budget and the frame count stop a picture that decodes too big.
        assert!(decode_frames(&gif(64, 64, 3)?, None, 64 * 64 * 4 * 2).is_err());
        assert!(decode_frames(&gif(1, 1, MAX_ANIMATION_FRAMES + 1)?, None, usize::MAX).is_err());
        assert!(decode_frames(b"not an image", None, 1 << 20).is_err());
        assert!(decode_frames_until(&gif(64, 64, 3)?, None, 1 << 20, || false).is_err());
        Ok(())
    }

    #[test]
    fn thumbnails_are_best_effort_and_bounded() -> Result<()> {
        let thumbnail = make_thumbnail(&png(640, 240)?).context("missing thumbnail")?;
        let decoded = decode_thumbnail(&thumbnail)?;
        assert_eq!((decoded.width(), decoded.height()), (320, 120));
        let mut jpeg = Cursor::new(Vec::new());
        DynamicImage::new_rgb8(64, 64).write_to(&mut jpeg, ImageFormat::Jpeg)?;
        assert!(make_thumbnail(jpeg.get_ref()).is_some());
        // Unsupported, oversized or broken images are uploaded without a thumbnail.
        assert!(make_thumbnail(b"GIF89a\x01\x00\x01\x00").is_none());
        assert!(make_thumbnail(b"RIFF\x00\x00\x00\x00WEBPVP8 ").is_none());
        assert!(make_thumbnail(&png(PICTURE_EDGE + 1, 1)?).is_none());
        assert!(make_thumbnail(&png(64, 64)?[..40]).is_none());

        // Received thumbnails are only small PNGs.
        assert!(decode_thumbnail(&png(321, 1)?).is_err());
        assert!(decode_thumbnail(jpeg.get_ref()).is_err());
        assert!(decode_thumbnail(&vec![0; MAX_THUMBNAIL_BYTES + 1]).is_err());
        Ok(())
    }

    /// A JPEG whose top half is red and bottom half blue, with an EXIF orientation.
    fn jpeg(width: u32, height: u32, orientation: Option<u16>) -> Result<Vec<u8>> {
        use image::{ImageEncoder, codecs::jpeg::JpegEncoder};
        let picture = image::RgbImage::from_fn(width, height, |_, y| {
            if y < height / 2 {
                image::Rgb([255, 0, 0])
            } else {
                image::Rgb([0, 0, 255])
            }
        });
        let mut bytes = Vec::new();
        let mut encoder = JpegEncoder::new(&mut bytes);
        if let Some(value) = orientation {
            // A little-endian TIFF header with one IFD entry: Orientation (0x0112), SHORT.
            let mut exif = b"II*\0\x08\0\0\0\x01\0\x12\x01\x03\0\x01\0\0\0".to_vec();
            exif.extend_from_slice(&value.to_le_bytes());
            exif.extend_from_slice(&[0; 6]);
            encoder.set_exif_metadata(exif)?;
        }
        encoder.write_image(
            picture.as_raw(),
            width,
            height,
            image::ExtendedColorType::Rgb8,
        )?;
        Ok(bytes)
    }

    #[test]
    fn phone_photos_get_an_upright_thumbnail() -> Result<()> {
        // A 24 MP photo, over the old 4096-pixel limit.
        let thumbnail = make_thumbnail(&jpeg(6000, 4000, None)?).context("missing thumbnail")?;
        let decoded = decode_thumbnail(&thumbnail)?;
        assert_eq!((decoded.width(), decoded.height()), (320, 213));
        // Shown in full it shrinks to a screen-sized picture.
        let full = decode_frames(&jpeg(6000, 4000, None)?, None, 1 << 30)?;
        assert_eq!(full[0].buffer().dimensions(), (3000, 2000));

        // Rotated 90° clockwise by EXIF: portrait, with the red top now on the right.
        let thumbnail = make_thumbnail(&jpeg(600, 400, Some(6))?).context("missing thumbnail")?;
        let decoded = decode_thumbnail(&thumbnail)?.into_rgb8();
        assert_eq!(decoded.dimensions(), (213, 320));
        let (left, right) = (decoded.get_pixel(20, 160), decoded.get_pixel(193, 160));
        assert!(left[2] > 200 && left[0] < 60, "left is {left:?}");
        assert!(right[0] > 200 && right[2] < 60, "right is {right:?}");
        let inline = decode_frames(&jpeg(600, 400, Some(6))?, Some(320), 1 << 20)?;
        assert_eq!(inline[0].buffer().dimensions(), (213, 320));
        Ok(())
    }

    #[test]
    fn jpeg_bombs_still_fail() -> Result<()> {
        // A baseline JPEG header claiming 65535×65535 pixels, with no data behind it.
        let mut bomb = jpeg(16, 16, None)?;
        let sof = bomb
            .windows(2)
            .position(|pair| pair == [0xff, 0xc0])
            .context("no SOF")?;
        bomb[sof + 5..sof + 9].copy_from_slice(&[0xff; 4]);
        assert!(decode_jpeg(&bomb, THUMBNAIL_EDGE).is_err());
        assert!(make_thumbnail(&bomb).is_none());
        assert!(decode_frames(&bomb, None, usize::MAX).is_err());
        // 8200×8200 is over the pixel limit, though each edge is allowed.
        let header = &mut jpeg(16, 16, None)?;
        header[sof + 5..sof + 9].copy_from_slice(&[0x20, 0x08, 0x20, 0x08]);
        assert!(decode_jpeg(header, THUMBNAIL_EDGE).is_err());
        assert!(make_thumbnail(header).is_none());
        Ok(())
    }
}
