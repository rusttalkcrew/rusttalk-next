use std::{
    fs::{File, OpenOptions},
    path::Path,
};

use anyhow::{Context, Result, anyhow};
use keyring::{Entry, Error};
use rusttalk_core::identity::{DeviceCertificate, DeviceIdentity};
use zeroize::Zeroizing;

/// The profile's keystore entry, held under a file lock for the app's lifetime.
pub struct Keystore {
    entry: Entry,
    _lock: File,
}

impl Keystore {
    /// Lock the profile and read its stored identity, if there is one.
    pub fn open(config: &Path, service: &str) -> Result<(Self, Option<DeviceIdentity>)> {
        let lock = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(config.join("identity.lock"))
            .context("open identity lock")?;
        lock.try_lock()
            .context("RustTalk is already open with this profile")?;

        Entry::store_status()
            .as_ref()
            .map_err(|error| anyhow!("open OS keystore: {error}"))?;
        let entry = Entry::new(service, "identity-v1").context("open OS keystore entry")?;
        let identity = match entry.get_secret() {
            Ok(secret) => Some(load(&Zeroizing::new(secret))?),
            Err(Error::NoEntry) => None,
            Err(error) => return Err(error).context("read identity from OS keystore"),
        };
        Ok((Self { entry, _lock: lock }, identity))
    }

    pub fn save(&self, identity: &DeviceIdentity) -> Result<()> {
        let certificate = identity.certificate.to_bytes();
        let mut secret = Zeroizing::new(Vec::with_capacity(128 + certificate.len()));
        secret.extend_from_slice(identity.signing_key.as_bytes());
        secret.extend_from_slice(identity.agreement_key.as_bytes());
        secret.extend_from_slice(identity.sync_key.as_ref());
        secret.extend_from_slice(identity.certificate.person_id());
        secret.extend_from_slice(&certificate);
        self.entry
            .set_secret(&secret)
            .context("save identity to OS keystore")
    }

    pub fn delete(&self) -> Result<()> {
        self.entry
            .delete_credential()
            .context("remove identity from OS keystore")
    }
}

fn load(secret: &[u8]) -> Result<DeviceIdentity> {
    let (keys, certificate) = secret
        .split_first_chunk::<128>()
        .context("truncated OS keystore identity")?;
    let [signing, agreement, sync, person_id] = keys.as_chunks::<32>().0 else {
        unreachable!("128 bytes contains four 32-byte keys");
    };
    let certificate =
        DeviceCertificate::verify(*person_id, certificate).context("verify stored certificate")?;
    DeviceIdentity::from_key_bytes(signing, agreement, sync, certificate)
        .context("load stored device keys")
}
