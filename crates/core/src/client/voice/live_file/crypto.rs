use anyhow::{Context, Result, ensure};
use chacha20poly1305::{
    KeyInit, XChaCha20Poly1305, XNonce,
    aead::{Aead, Payload},
};
use sha2::{Digest, Sha256};
use zeroize::Zeroizing;

use crate::messages::{LIVE_CHUNK_SIZE, LiveAttachment};

pub(super) const TAG_SIZE: usize = 16;

const fn chunks(size: u64) -> u64 {
    if size == 0 {
        1
    } else {
        size.div_ceil(LIVE_CHUNK_SIZE as u64)
    }
}

pub(super) const fn wire_size(size: u64) -> u64 {
    size + chunks(size) * TAG_SIZE as u64
}

pub(super) fn nonce_prefix(device: &[u8; 32], acceptance: &[u8; 16]) -> [u8; 16] {
    Sha256::new()
        .chain_update(b"rusttalk-file-nonce-v1")
        .chain_update(device)
        .chain_update(acceptance)
        .finalize()[..16]
        .try_into()
        .expect("16-byte prefix")
}

pub(super) struct FileCipher {
    cipher: XChaCha20Poly1305,
    transfer: [u8; 32],
    prefix: [u8; 16],
    size: u64,
    index: u64,
}

impl FileCipher {
    pub(super) fn new(file: LiveAttachment, prefix: [u8; 16]) -> Result<Self> {
        file.validate()
            .context("invalid live file offer (size or metadata)")?;
        Ok(Self {
            cipher: XChaCha20Poly1305::new((&*file.key).into()),
            transfer: file.id,
            prefix,
            size: file.size,
            index: 0,
        })
    }

    pub(super) fn next_chunk(&self) -> Option<(usize, bool)> {
        let count = chunks(self.size);
        (self.index < count).then(|| {
            let left = self.size - self.index * LIVE_CHUNK_SIZE as u64;
            (
                left.min(LIVE_CHUNK_SIZE as u64) as usize,
                self.index + 1 == count,
            )
        })
    }

    pub(super) fn seal(&mut self, plaintext: &[u8]) -> Result<Vec<u8>> {
        let (len, final_chunk) = self.next_chunk().context("file already sealed")?;
        ensure!(plaintext.len() == len, "invalid plaintext chunk length");
        let (nonce, aad) = self.context(final_chunk);
        let sealed = self
            .cipher
            .encrypt(
                &nonce,
                Payload {
                    msg: plaintext,
                    aad: &aad,
                },
            )
            .map_err(|_| anyhow::anyhow!("file chunk encryption failed"))?;
        self.index += 1;
        Ok(sealed)
    }

    pub(super) fn open(&mut self, sealed: &[u8]) -> Result<Zeroizing<Vec<u8>>> {
        let (len, final_chunk) = self.next_chunk().context("excess file stream bytes")?;
        ensure!(
            sealed.len() == len + TAG_SIZE,
            "invalid ciphertext chunk length"
        );
        let (nonce, aad) = self.context(final_chunk);
        let opened = self
            .cipher
            .decrypt(
                &nonce,
                Payload {
                    msg: sealed,
                    aad: &aad,
                },
            )
            .map_err(|_| anyhow::anyhow!("file chunk authentication failed"))?;
        self.index += 1;
        Ok(Zeroizing::new(opened))
    }

    pub(super) fn finish(&self) -> Result<()> {
        ensure!(
            self.next_chunk().is_none(),
            "file stream ended before the final authenticated chunk"
        );
        Ok(())
    }

    fn context(&self, final_chunk: bool) -> (XNonce, Vec<u8>) {
        let index = self.index.to_be_bytes();
        let mut nonce = [0; 24];
        nonce[..16].copy_from_slice(&self.prefix);
        nonce[16..].copy_from_slice(&index);
        let aad = [
            b"rusttalk-file-chunk-v1".as_slice(),
            &self.transfer,
            &index,
            &[u8::from(final_chunk)],
        ]
        .concat();
        (nonce.into(), aad)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{keys::Key, messages::MAX_LIVE_FILE_SIZE};

    fn attachment(size: u64) -> LiveAttachment {
        LiveAttachment {
            id: [1; 32],
            key: Key::new([2; 32]),
            name: "example.bin".to_owned(),
            mime_type: "application/octet-stream".to_owned(),
            size,
            chunk_size: LIVE_CHUNK_SIZE as u32,
        }
    }

    #[test]
    fn chunks_authenticate_order_end_and_transfer() {
        let file = attachment(2 * LIVE_CHUNK_SIZE as u64);
        let prefix = [3; 16];
        let mut sender = FileCipher::new(file.clone(), prefix).unwrap();
        let first = sender.seal(&[4; LIVE_CHUNK_SIZE]).unwrap();
        let last = sender.seal(&[5; LIVE_CHUNK_SIZE]).unwrap();
        sender.finish().unwrap();

        let mut receiver = FileCipher::new(file.clone(), prefix).unwrap();
        assert!(receiver.open(&last).is_err(), "reordered chunks");
        let mut tampered = first.clone();
        tampered[0] ^= 1;
        assert!(receiver.open(&tampered).is_err(), "tampered chunk");
        assert_eq!(**receiver.open(&first).unwrap(), [4; LIVE_CHUNK_SIZE]);
        assert!(receiver.finish().is_err(), "missing final chunk");
        assert!(receiver.open(&first).is_err(), "replayed non-final chunk");
        assert_eq!(**receiver.open(&last).unwrap(), [5; LIVE_CHUNK_SIZE]);
        receiver.finish().unwrap();
        assert!(
            receiver.open(&last).is_err(),
            "extra bytes after final chunk"
        );

        let different = LiveAttachment {
            id: [9; 32],
            ..file
        };
        assert!(
            FileCipher::new(different, prefix)
                .unwrap()
                .open(&first)
                .is_err()
        );
    }

    #[test]
    fn file_size_bounds_and_empty_final_chunk() {
        assert!(FileCipher::new(attachment(MAX_LIVE_FILE_SIZE + 1), [0; 16]).is_err());
        let file = attachment(MAX_LIVE_FILE_SIZE);
        let mut receiver = FileCipher::new(file.clone(), [0; 16]).unwrap();
        // Start at the last frame to exercise the 2 GiB boundary without materializing it.
        receiver.index = chunks(MAX_LIVE_FILE_SIZE) - 1;
        let mut sender = FileCipher::new(file, [0; 16]).unwrap();
        sender.index = receiver.index;
        let sealed = sender.seal(&[0; LIVE_CHUNK_SIZE]).unwrap();
        assert!(receiver.open(&[sealed.as_slice(), &[0]].concat()).is_err());
        receiver.open(&sealed).unwrap();
        assert!(receiver.open(&[0]).is_err());
        receiver.finish().unwrap();
        assert_eq!(wire_size(MAX_LIVE_FILE_SIZE), MAX_LIVE_FILE_SIZE + 524_288);

        let mut empty = FileCipher::new(attachment(0), [0; 16]).unwrap();
        assert!(empty.finish().is_err());
        let sealed = empty.seal(&[]).unwrap();
        assert_eq!(sealed.len(), TAG_SIZE);
        let mut receiver = FileCipher::new(attachment(0), [0; 16]).unwrap();
        assert!(receiver.open(&sealed).unwrap().is_empty());
        receiver.finish().unwrap();
    }
}
