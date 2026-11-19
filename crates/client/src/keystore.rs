use std::{
    fs::OpenOptions,
    io::{self, Write},
    path::Path,
};

use anyhow::{Context, Result, anyhow};
use keyring::{Entry, Error};
use rusttalk_core::identity::{DeviceCertificate, DeviceIdentity};
use zeroize::Zeroizing;

pub fn load_or_create(config: &Path, service: &str) -> Result<DeviceIdentity> {
    let lock = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(config.join("identity.lock"))
        .context("open identity lock")?;
    lock.try_lock().context("acquire identity lock")?;

    Entry::store_status()
        .as_ref()
        .map_err(|error| anyhow!("open OS keystore: {error}"))?;
    let entry = Entry::new(service, "identity-v1").context("open OS keystore entry")?;
    match entry.get_secret() {
        Ok(secret) => load(&Zeroizing::new(secret)),
        Err(Error::NoEntry) => create(&entry),
        Err(error) => Err(error).context("read identity from OS keystore"),
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

fn create(entry: &Entry) -> Result<DeviceIdentity> {
    let (words, identity) = DeviceIdentity::create("Desktop").context("create identity")?;
    let certificate = identity.certificate.to_bytes();
    let mut secret = Zeroizing::new(Vec::with_capacity(128 + certificate.len()));
    secret.extend_from_slice(identity.signing_key.as_bytes());
    secret.extend_from_slice(identity.agreement_key.as_bytes());
    secret.extend_from_slice(identity.sync_key.as_ref());
    secret.extend_from_slice(identity.certificate.person_id());
    secret.extend_from_slice(&certificate);

    // shortcut: words printed to terminal until the client UI exists
    let mut stdout = io::stdout().lock();
    writeln!(stdout, "Recovery words (shown once):\n{}", words.as_str())
        .and_then(|()| stdout.flush())
        .context("display recovery words")?;
    drop(words);
    entry
        .set_secret(&secret)
        .context("save identity to OS keystore")?;
    Ok(identity)
}
