//! Wire framing for the opaque per-person sync blob.

pub const PATH: &str = "/v1/me/sync";
pub const MAX_CIPHERTEXT: usize = 256 * 1024;
pub const MAX_BODY: usize = 8 + MAX_CIPHERTEXT;
pub const NONCE_SIZE: usize = 24;
pub const MIN_CIPHERTEXT: usize = NONCE_SIZE + 16;

pub fn split_blob(body: &[u8]) -> Option<(i64, &[u8])> {
    let (header, ciphertext) = body.split_first_chunk()?;
    let version = i64::from_be_bytes(*header);
    (version > 0 && (MIN_CIPHERTEXT..=MAX_CIPHERTEXT).contains(&ciphertext.len()))
        .then_some((version, ciphertext))
}
