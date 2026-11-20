use std::collections::HashMap;

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
// "-9223372036854775808." plus base64url of nonce, name and tag.
const MAX_WIRE_BYTES: usize = 21 + (MAX_NAME_BYTES + 24 + 16).div_ceil(3) * 4;

/// Name key for one epoch's media key.
pub(super) fn derive(media_key: &Key) -> Key {
    let mut key = Zeroizing::new([0; 32]);
    Hkdf::<Sha256>::new(None, media_key.as_ref())
        .expand(LABEL, key.as_mut())
        .expect("32-byte HKDF-SHA256 output");
    key
}

fn aad(epoch: i64) -> Vec<u8> {
    [LABEL, &epoch.to_be_bytes()].concat()
}

/// `epoch.base64url(nonce || ciphertext)`, so receivers pick the key of the stated epoch.
pub(super) fn encrypt(key: &Key, epoch: i64, name: &str) -> Result<String> {
    let name = &name[..name.floor_char_boundary(MAX_NAME_BYTES)];
    let sealed = keys::seal(key, &aad(epoch), name.as_bytes())?;
    Ok(format!("{epoch}.{}", auth::encode(&sealed)))
}

pub(super) fn decrypt(keys: &HashMap<i64, Key>, wire: &str) -> Option<String> {
    if wire.len() > MAX_WIRE_BYTES {
        return None;
    }
    let (epoch, sealed) = wire.split_once('.')?;
    let epoch: i64 = epoch.parse().ok()?;
    let bytes = auth::decode_bytes(sealed).ok()?;
    let name = keys::open(keys.get(&epoch)?, &aad(epoch), &bytes).ok()?;
    let name = String::from_utf8(name).ok()?;
    (!name.trim().is_empty() && name.len() <= MAX_NAME_BYTES).then_some(name)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn app_track_name_authentication() {
        let old = derive(&Zeroizing::new([1; 32]));
        let new = derive(&Zeroizing::new([2; 32]));
        let keys = HashMap::from([(3, old.clone()), (4, new.clone())]);
        let name = "Example Player ♫";
        let wire = encrypt(&old, 3, name).unwrap();
        assert!(wire.starts_with("3."));
        assert_ne!(wire, encrypt(&old, 3, name).unwrap());
        // A share started before a rotation still decrypts with its epoch's key.
        assert_eq!(decrypt(&keys, &wire).as_deref(), Some(name));
        assert_eq!(decrypt(&HashMap::from([(4, new.clone())]), &wire), None);
        // Relabelling the epoch fails, even if both epochs held the same key.
        let relabelled = wire.replacen("3.", "4.", 1);
        assert_eq!(decrypt(&keys, &relabelled), None);
        let same = HashMap::from([(3, old.clone()), (4, old.clone())]);
        assert_eq!(decrypt(&same, &relabelled), None);
        let mut tampered = auth::decode_bytes(&wire[2..]).unwrap();
        tampered[24] ^= 1;
        assert_eq!(
            decrypt(&keys, &format!("3.{}", auth::encode(&tampered))),
            None
        );
        assert_eq!(decrypt(&keys, name), None);
        assert_eq!(decrypt(&keys, &wire[2..]), None);
        let sealed = keys::seal(&old, &aad(3), "x".repeat(400).as_bytes()).unwrap();
        let oversized = format!("3.{}", auth::encode(&sealed));
        assert!(oversized.len() > MAX_WIRE_BYTES);
        assert_eq!(decrypt(&keys, &oversized), None);
        assert_eq!(decrypt(&keys, &encrypt(&old, 3, "   ").unwrap()), None);
        let long = "♫".repeat(100);
        let wire = encrypt(&new, i64::MIN, &long).unwrap();
        let keys = HashMap::from([(i64::MIN, new)]);
        assert_eq!(decrypt(&keys, &wire).unwrap().len(), 126);
    }
}
