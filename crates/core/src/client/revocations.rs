//! Root-signed, permanent device revocations shared by all connections in a profile.
use std::{
    collections::BTreeSet,
    ffi::OsStr,
    fmt::Write,
    fs, io,
    io::Read,
    path::{Path, PathBuf},
};

use super::sync::{Peer, Snapshot};
use crate::{
    auth::{decode_bytes, encode},
    identity::{DeviceCertificate, PersonId, REVOCATIONS_PER_PERSON, Revocation},
};
use anyhow::{Result, ensure};

// A server can also invent whole members with valid certificates, and change them between replies.
const MAX_PEOPLE: usize = 1024;
pub(super) const MAX_PER_REPLY: usize = 4096;

#[cfg(feature = "voice")]
pub(super) static CHANGED: std::sync::LazyLock<tokio::sync::watch::Sender<()>> =
    std::sync::LazyLock::new(tokio::sync::watch::Sender::default);

fn key_filename(name: &OsStr) -> bool {
    name.to_str()
        .is_some_and(|s| s.len() == 64 && s.bytes().all(|b| b.is_ascii_hexdigit()))
}

fn count_keys(dir: &Path) -> Result<usize> {
    let mut count = 0;
    for entry in fs::read_dir(dir)? {
        if key_filename(&entry?.file_name()) {
            count += 1;
        }
    }
    Ok(count)
}

pub(super) fn verify(person: &PersonId, encoded: &str) -> Result<Revocation> {
    ensure!(encoded.len() <= 256, "oversized device revocation");
    Ok(Revocation::verify(*person, &decode_bytes(encoded)?)?)
}

fn read_record(person: &PersonId, path: &Path) -> Result<Revocation> {
    let mut bytes = Vec::new();
    fs::File::open(path)?.take(513).read_to_end(&mut bytes)?;
    ensure!(bytes.len() <= 512, "oversized saved device revocation");
    let encoded: String = serde_json::from_slice(&bytes)?;
    let record = verify(person, &encoded)?;
    ensure!(
        path.file_name() == Some(OsStr::new(&filename(record.device_signing_key()))),
        "device key differs from filename"
    );
    Ok(record)
}

#[derive(Clone)]
pub struct Revocations(PathBuf);

// Lowercase hex also distinguishes all keys on case-insensitive filesystems.
fn filename(key: &[u8; 32]) -> String {
    let mut name = String::with_capacity(64);
    for byte in key {
        write!(name, "{byte:02x}").expect("writing to String");
    }
    name
}

impl Revocations {
    pub fn in_profile(data: &Path) -> Self {
        Self(data.join("revocations"))
    }

    pub(super) fn check(&self, person: &PersonId, device: &[u8; 32]) -> Result<()> {
        let file = self.0.join(filename(person)).join(filename(device));
        ensure!(!file.try_exists()?, "device revoked");
        Ok(())
    }

    pub(super) fn allows(&self, certificate: &DeviceCertificate) -> bool {
        self.check(
            certificate.person_id(),
            certificate.signing_key().as_bytes(),
        )
        .is_ok()
    }

    /// Invalid signatures have no effect, including on other records in the same reply.
    pub fn observe(&self, person: &PersonId, encoded: &str) -> Result<bool> {
        let Ok(record) = verify(person, encoded) else {
            tracing::warn!("ignored invalid device revocation");
            return Ok(false);
        };
        self.remember(&record)
    }

    pub(super) fn remember(&self, record: &Revocation) -> Result<bool> {
        let dir = self.0.join(filename(record.person_id()));
        let file = dir.join(filename(record.device_signing_key()));
        if file.try_exists()? && read_record(record.person_id(), &file).is_ok() {
            return Ok(false);
        }
        fs::create_dir_all(&self.0)?;
        let lock = fs::File::options()
            .create(true)
            .truncate(false)
            .write(true)
            .open(self.0.join("store.lock"))?;
        lock.lock()?;
        if file.try_exists()? && read_record(record.person_id(), &file).is_ok() {
            return Ok(false);
        }
        if !dir.try_exists()? {
            ensure!(
                count_keys(&self.0)? < MAX_PEOPLE,
                "device revocation profile limit reached"
            );
            fs::create_dir(&dir)?;
        }
        ensure!(
            file.try_exists()? || count_keys(&dir)? < REVOCATIONS_PER_PERSON,
            "device revocation owner limit reached"
        );
        super::save_json(&dir, &file, &encode(&record.to_bytes()))?;
        #[cfg(feature = "voice")]
        CHANGED.send_replace(());
        #[cfg(unix)]
        for path in [&dir, &self.0, self.0.parent().expect("profile directory")] {
            fs::File::open(path)?.sync_all()?;
        }
        Ok(true)
    }

    /// A damaged tombstone still blocks its key by filename. Never relay its contents.
    fn records(&self, person: &PersonId) -> Result<Vec<Revocation>> {
        let entries = match fs::read_dir(self.0.join(filename(person))) {
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
            result => result?,
        };
        let mut records = Vec::new();
        for entry in entries {
            let read = || -> Result<Option<Revocation>> {
                let entry = entry?;
                if !key_filename(&entry.file_name()) {
                    return Ok(None);
                }
                Ok(Some(read_record(person, &entry.path())?))
            };
            match read() {
                Ok(Some(record)) => records.push(record),
                Ok(None) => (),
                Err(_) => tracing::warn!("skipped unreadable or invalid saved device revocation"),
            }
        }
        Ok(records)
    }

    /// Merge this owner's records from every encrypted snapshot; publish new, verified records
    /// to each available server and return only verified records for the next encrypted blob.
    pub async fn relay<'a>(
        &self,
        person: &PersonId,
        snapshots: impl IntoIterator<Item = &'a Snapshot>,
        peers: &[Peer<'_>],
    ) -> Result<BTreeSet<String>> {
        for snapshot in snapshots {
            ensure!(
                snapshot.state.revocations.len() <= MAX_PER_REPLY,
                "oversized sync revocation list"
            );
            for encoded in &snapshot.state.revocations {
                self.observe(person, encoded)?;
            }
        }
        let mut verified = BTreeSet::new();
        for record in self.records(person)? {
            for (server, session) in peers {
                if let Err(error) = server.publish_revocation(session, &record).await {
                    tracing::warn!(%error, "publishing device revocation failed");
                }
            }
            verified.insert(encode(&record.to_bytes()));
        }
        Ok(verified)
    }

    #[cfg(feature = "voice")]
    pub(super) fn voice_identities(
        &self,
        server: &[u8; 32],
    ) -> Result<std::collections::HashSet<String>> {
        let people = match fs::read_dir(&self.0) {
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Default::default()),
            result => result?,
        };
        let mut blocked = std::collections::HashSet::new();
        for person in people {
            let person = person?;
            if !key_filename(&person.file_name()) {
                continue;
            }
            for device in fs::read_dir(person.path())? {
                let name = device?.file_name();
                if !key_filename(&name) {
                    continue;
                }
                let name = name.to_str().expect("hexadecimal device key");
                let mut key = [0; 32];
                for (index, byte) in key.iter_mut().enumerate() {
                    *byte = u8::from_str_radix(&name[2 * index..2 * index + 2], 16)?;
                }
                blocked.insert(crate::auth::voice_identity(server, &key));
            }
        }
        Ok(blocked)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::identity::{DeviceIdentity, revoke_device};

    #[test]
    fn corrupt_or_misfiled_records_do_not_block_other_records_or_get_relayed() {
        let dir = tempfile::tempdir().unwrap();
        let (words, cert) = DeviceIdentity::create("owner").unwrap();
        let person = cert.certificate.person_id();
        let record = revoke_device(&words, cert.certificate.signing_key().to_bytes()).unwrap();
        let records = Revocations::in_profile(dir.path());
        records.remember(&record).unwrap();
        let folder = records.0.join(filename(person));
        fs::write(folder.join(filename(&[8; 32])), "not json").unwrap();
        fs::write(
            folder.join(filename(&[9; 32])),
            serde_json::to_vec(&encode(&record.to_bytes())).unwrap(),
        )
        .unwrap();
        let loaded = records.records(person).unwrap();
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].to_bytes(), record.to_bytes());
        assert!(records.check(person, &[8; 32]).is_err());
    }

    #[test]
    fn full_owner_cannot_grow_the_store_or_evict_an_old_key() {
        let dir = tempfile::tempdir().unwrap();
        let (words, cert) = DeviceIdentity::create("owner").unwrap();
        let person = cert.certificate.person_id();
        let records = Revocations::in_profile(dir.path());
        let folder = records.0.join(filename(person));
        fs::create_dir_all(&folder).unwrap();
        for i in 0..REVOCATIONS_PER_PERSON {
            fs::write(folder.join(filename(&[i as u8; 32])), []).unwrap();
        }
        let record = revoke_device(&words, cert.certificate.signing_key().to_bytes()).unwrap();
        assert!(records.remember(&record).is_err());
        assert_eq!(count_keys(&folder).unwrap(), REVOCATIONS_PER_PERSON);
        assert!(records.check(person, &[0; 32]).is_err());
    }

    #[test]
    fn verified_revocations_survive_replay_and_restart() {
        let dir = tempfile::tempdir().unwrap();
        let (words, b1) = DeviceIdentity::create("B1").unwrap();
        let b2 = DeviceIdentity::add_device(&words, "B2").unwrap();
        let (other, _) = DeviceIdentity::create("other").unwrap();
        let person = b1.certificate.person_id();
        let device = b2.certificate.signing_key().to_bytes();
        let records = Revocations::in_profile(dir.path());
        let signed = |words| encode(&revoke_device(words, device).unwrap().to_bytes());
        assert!(!records.observe(person, &signed(&other)).unwrap());
        assert!(records.allows(&b2.certificate));
        let valid = signed(&words);
        assert!(records.observe(person, &valid).unwrap());
        assert!(!records.observe(person, &valid).unwrap());
        let restarted = Revocations::in_profile(dir.path());
        let stale: crate::keys::KeyState =
            serde_json::from_str(r#"{"scopes":[],"envelopes":[],"pending":[]}"#).unwrap();
        for (owner, record) in stale.revocations {
            restarted
                .observe(&crate::auth::decode(&owner).unwrap(), &record)
                .unwrap();
        }
        assert!(restarted.allows(&b1.certificate));
        assert!(!restarted.allows(&b2.certificate));
        assert_eq!(
            encode(&restarted.records(person).unwrap()[0].to_bytes()),
            valid
        );
    }
}
