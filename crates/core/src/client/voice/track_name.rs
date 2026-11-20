use anyhow::Result;
use hkdf::Hkdf;
use sha2::Sha256;
use zeroize::Zeroizing;

use crate::{
    auth,
    keys::{self, Key},
};

const LABEL: &[u8] = b"rusttalk-track-name-v1";
const MAX_NAME_BYTES: usize = 128;
const MAX_WIRE_BYTES: usize = (MAX_NAME_BYTES + 24 + 16).div_ceil(3) * 4;

pub(super) fn derive(media_key: &Key) -> Key {
    let mut key = Zeroizing::new([0; 32]);
    Hkdf::<Sha256>::new(None, media_key.as_ref())
        .expand(LABEL, key.as_mut())
        .expect("32-byte HKDF-SHA256 output");
    key
}

pub(super) fn encrypt(key: &Key, name: &str) -> Result<String> {
    let name = &name[..name.floor_char_boundary(MAX_NAME_BYTES)];
    Ok(auth::encode(&keys::seal(key, LABEL, name.as_bytes())?))
}

pub(super) fn decrypt(key: &Key, wire: &str) -> Option<String> {
    if wire.len() > MAX_WIRE_BYTES {
        return None;
    }
    let bytes = auth::decode_bytes(wire).ok()?;
    let name = String::from_utf8(keys::open(key, LABEL, &bytes).ok()?).ok()?;
    (!name.trim().is_empty() && name.len() <= MAX_NAME_BYTES).then_some(name)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn app_track_name_authentication() {
        let key = derive(&Zeroizing::new([1; 32]));
        let wrong = derive(&Zeroizing::new([2; 32]));
        let name = "Example Player ♫";
        let wire = encrypt(&key, name).unwrap();
        assert_ne!(wire, encrypt(&key, name).unwrap());
        assert_eq!(decrypt(&key, &wire).as_deref(), Some(name));
        assert_eq!(decrypt(&wrong, &wire), None);
        let mut tampered = auth::decode_bytes(&wire).unwrap();
        tampered[24] ^= 1;
        assert_eq!(decrypt(&key, &auth::encode(&tampered)), None);
        assert_eq!(decrypt(&key, name), None);
        let long = "♫".repeat(100);
        assert_eq!(
            decrypt(&key, &encrypt(&key, &long).unwrap()).unwrap().len(),
            126
        );
    }
}
