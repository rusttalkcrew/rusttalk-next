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
use image::{DynamicImage, ImageFormat, ImageReader, Limits};
use reqwest::{Body, Method, StatusCode, header::CONTENT_TYPE};
use serde::Deserialize;
use tokio::time;
use zeroize::Zeroizing;

use crate::{
    auth::{decode, encode},
    keys::random_key,
    messages::{Attachment, BlobRef, CHUNK_BYTES, MAX_CIPHERTEXT, MAX_THUMBNAIL_BYTES, TAG_BYTES},
};

use super::KeyClient;

/// A transfer fails when no bytes move for this long, so slow links still finish.
const TRANSFER_IDLE: Duration = Duration::from_secs(30);
/// The longest one file transfer may take.
const TRANSFER_LIMIT: Duration = Duration::from_secs(15 * 60);
/// Upload body piece size; each piece taken by the connection counts as progress.
const UPLOAD_PIECE: usize = 64 * 1024;

/// The API refused an upload. `code` is its error code, such as `file_member_quota`.
#[derive(Debug, thiserror::Error)]
#[error("upload rejected with {status}: {code}")]
pub struct UploadRejected {
    pub status: StatusCode,
    pub code: String,
}
const THUMBNAIL_EDGE: u32 = 320;

impl KeyClient<'_> {
    /// Encrypt and upload a file and, for PNG/JPEG, an independently encrypted PNG thumbnail.
    pub async fn upload_file(
        &self,
        channel: &[u8; 32],
        name: &str,
        media_type: &str,
        bytes: &[u8],
    ) -> Result<Attachment> {
        let mut attachment = Attachment {
            file: blob_ref(bytes.len())?,
            name: name.into(),
            media_type: media_type.into(),
            thumbnail: None,
        };
        attachment.validate()?;
        let thumbnail = make_thumbnail(bytes);
        if let Some(png) = &thumbnail {
            attachment.thumbnail = Some(blob_ref(png.len())?);
        }
        self.upload_blob(channel, &attachment.file, bytes).await?;
        if let (Some(reference), Some(bytes)) = (&attachment.thumbnail, thumbnail)
            && let Err(error) = self.upload_blob(channel, reference, &bytes).await
        {
            if let Err(cleanup) = self.delete_file(&attachment.file.id).await {
                tracing::warn!(status = ?cleanup.downcast_ref::<reqwest::Error>().and_then(reqwest::Error::status),
                    "could not remove file after thumbnail upload failed");
            }
            return Err(error);
        }
        Ok(attachment)
    }

    async fn upload_blob(
        &self,
        channel: &[u8; 32],
        reference: &BlobRef,
        bytes: &[u8],
    ) -> Result<()> {
        #[derive(Deserialize)]
        struct Uploaded {
            id: String,
        }
        let ciphertext = seal(reference, bytes)?;
        #[derive(Deserialize)]
        struct Problem {
            error: String,
        }
        let taken = Arc::new(AtomicUsize::new(0));
        let pieces = {
            let taken = taken.clone();
            let ends = (UPLOAD_PIECE..ciphertext.len() + UPLOAD_PIECE).step_by(UPLOAD_PIECE);
            stream::iter(ends.map(move |end| {
                let start = end - UPLOAD_PIECE;
                taken.fetch_add(1, Ordering::Relaxed);
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
        let response = loop {
            let before = taken.load(Ordering::Relaxed);
            match time::timeout(TRANSFER_IDLE, &mut send).await {
                Ok(response) => break response?,
                Err(_) => ensure!(taken.load(Ordering::Relaxed) != before, "upload stalled"),
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

    /// Download a file or thumbnail and return bytes only after authenticating the entire blob.
    pub async fn download_file(&self, reference: &BlobRef) -> Result<Vec<u8>> {
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

/// Decode only JPEG or PNG, with dimension and allocation limits for hostile input.
pub fn decode_image(bytes: &[u8]) -> Result<DynamicImage> {
    decode_limited(
        bytes,
        &[ImageFormat::Png, ImageFormat::Jpeg],
        4096,
        64 * 1024 * 1024,
    )
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
    reader.limits(limits);
    Ok(reader.decode()?)
}

/// A PNG thumbnail for a decodable PNG or JPEG. Anything else, including images
/// over the decode limits, is still sent, just without a thumbnail.
fn make_thumbnail(bytes: &[u8]) -> Option<Vec<u8>> {
    let image = decode_image(bytes).ok()?;
    let mut png = Cursor::new(Vec::new());
    DynamicImage::ImageRgba8(image.thumbnail(THUMBNAIL_EDGE, THUMBNAIL_EDGE).into_rgba8())
        .write_to(&mut png, ImageFormat::Png)
        .ok()?;
    let png = png.into_inner();
    (png.len() <= MAX_THUMBNAIL_BYTES).then_some(png)
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
        DynamicImage::new_rgb8(4097, 1).write_to(&mut png, ImageFormat::Png)?;
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
        assert!(make_thumbnail(&png(4097, 1)?).is_none());
        assert!(make_thumbnail(&png(64, 64)?[..40]).is_none());

        // Received thumbnails are only small PNGs.
        assert!(decode_thumbnail(&png(321, 1)?).is_err());
        assert!(decode_thumbnail(jpeg.get_ref()).is_err());
        assert!(decode_thumbnail(&vec![0; MAX_THUMBNAIL_BYTES + 1]).is_err());
        Ok(())
    }
}
