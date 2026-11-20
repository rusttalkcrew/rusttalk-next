use std::collections::HashMap;

use anyhow::Result;
use hkdf::Hkdf;
use sha2::Sha256;
use zeroize::Zeroizing;

use crate::{
    auth,
    keys::{self, Key},
};

pub(super) const APP_AUDIO: &[u8] = b"rusttalk-track-name-v1";
pub(super) const SCREEN: &[u8] = b"rusttalk-screen-track-name-v1";
const MAX_NAME_BYTES: usize = 128;
// "-9223372036854775808." plus base64url of nonce, name and tag.
const MAX_WIRE_BYTES: usize = 21 + (MAX_NAME_BYTES + 24 + 16).div_ceil(3) * 4;

/// Name key for one epoch's media key.
pub(super) fn derive(media_key: &Key, label: &[u8]) -> Key {
    let mut key = Zeroizing::new([0; 32]);
    Hkdf::<Sha256>::new(None, media_key.as_ref())
        .expand(label, key.as_mut())
        .expect("32-byte HKDF-SHA256 output");
    key
}

fn aad(label: &[u8], epoch: i64) -> Vec<u8> {
    [label, &epoch.to_be_bytes()].concat()
}

/// `epoch.base64url(nonce || ciphertext)`, so receivers pick the key of the stated epoch.
pub(super) fn encrypt(key: &Key, label: &[u8], epoch: i64, name: &str) -> Result<String> {
    let name = &name[..name.floor_char_boundary(MAX_NAME_BYTES)];
    let sealed = keys::seal(key, &aad(label, epoch), name.as_bytes())?;
    Ok(format!("{epoch}.{}", auth::encode(&sealed)))
}

pub(super) fn decrypt(keys: &HashMap<i64, Key>, label: &[u8], wire: &str) -> Option<String> {
    if wire.len() > MAX_WIRE_BYTES {
        return None;
    }
    let (epoch, sealed) = wire.split_once('.')?;
    let epoch: i64 = epoch.parse().ok()?;
    let bytes = auth::decode_bytes(sealed).ok()?;
    let name = keys::open(keys.get(&epoch)?, &aad(label, epoch), &bytes).ok()?;
    let name = String::from_utf8(name).ok()?;
    (!name.trim().is_empty() && name.len() <= MAX_NAME_BYTES).then_some(name)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn app_track_name_authentication() {
        let old = derive(&Zeroizing::new([1; 32]), APP_AUDIO);
        let new = derive(&Zeroizing::new([2; 32]), APP_AUDIO);
        let keys = HashMap::from([(3, old.clone()), (4, new.clone())]);
        let name = "Example Player ♫";
        let wire = encrypt(&old, APP_AUDIO, 3, name).unwrap();
        assert!(wire.starts_with("3."));
        let screen_keys = HashMap::from([(3, derive(&Zeroizing::new([1; 32]), SCREEN))]);
        let screen = encrypt(&screen_keys[&3], SCREEN, 3, name).unwrap();
        assert_eq!(
            decrypt(&screen_keys, SCREEN, &screen).as_deref(),
            Some(name)
        );
        assert_eq!(decrypt(&screen_keys, SCREEN, &wire), None);
        assert_eq!(decrypt(&keys, APP_AUDIO, &screen), None);
        assert_ne!(wire, encrypt(&old, APP_AUDIO, 3, name).unwrap());
        // A share started before a rotation still decrypts with its epoch's key.
        assert_eq!(decrypt(&keys, APP_AUDIO, &wire).as_deref(), Some(name));
        assert_eq!(
            decrypt(&HashMap::from([(4, new.clone())]), APP_AUDIO, &wire),
            None
        );
        // Relabelling the epoch fails, even if both epochs held the same key.
        let relabelled = wire.replacen("3.", "4.", 1);
        assert_eq!(decrypt(&keys, APP_AUDIO, &relabelled), None);
        let same = HashMap::from([(3, old.clone()), (4, old.clone())]);
        assert_eq!(decrypt(&same, APP_AUDIO, &relabelled), None);
        let mut tampered = auth::decode_bytes(&wire[2..]).unwrap();
        tampered[24] ^= 1;
        assert_eq!(
            decrypt(&keys, APP_AUDIO, &format!("3.{}", auth::encode(&tampered))),
            None
        );
        assert_eq!(decrypt(&keys, APP_AUDIO, name), None);
        assert_eq!(decrypt(&keys, APP_AUDIO, &wire[2..]), None);
        let long = "♫".repeat(100);
        let wire = encrypt(&new, APP_AUDIO, i64::MIN, &long).unwrap();
        let keys = HashMap::from([(i64::MIN, new)]);
        assert_eq!(decrypt(&keys, APP_AUDIO, &wire).unwrap().len(), 126);
    }
}
