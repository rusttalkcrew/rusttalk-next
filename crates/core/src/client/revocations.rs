//! Root-signed, permanent device revocations shared by all connections in a profile.
use std::{
    collections::BTreeSet,
    fmt::Write,
    fs, io,
    path::{Path, PathBuf},
};

use crate::{
    auth::{decode_bytes, encode},
    identity::{DeviceCertificate, PersonId, Revocation},
};
use anyhow::{Result, ensure};

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
        let Ok(bytes) = decode_bytes(encoded) else {
            tracing::warn!("ignored malformed device revocation");
            return Ok(false);
        };
        let Ok(record) = Revocation::verify(*person, &bytes) else {
            tracing::warn!("ignored invalid device revocation signature");
            return Ok(false);
        };
        self.remember(&record)
    }

    pub(super) fn remember(&self, record: &Revocation) -> Result<bool> {
        let dir = self.0.join(filename(record.person_id()));
        let file = dir.join(filename(record.device_signing_key()));
        if file.try_exists()? {
            return Ok(false);
        }
        fs::create_dir_all(&dir)?;
        super::save_json(&dir, &file, &encode(&record.to_bytes()))?;
        #[cfg(unix)]
        for path in [&dir, &self.0, self.0.parent().expect("profile directory")] {
            fs::File::open(path)?.sync_all()?;
        }
        Ok(true)
    }

    /// Only the owner's records are carried in their sync blob or published in their name.
    pub fn records(&self, person: &PersonId) -> Result<BTreeSet<String>> {
        let entries = match fs::read_dir(self.0.join(filename(person))) {
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(BTreeSet::new()),
            result => result?,
        };
        let mut out = BTreeSet::new();
        for entry in entries {
            let entry = entry?;
            // Atomic writers can have a temporary file here.
            if entry.file_name().len() == 64 {
                out.insert(serde_json::from_slice(&fs::read(entry.path())?)?);
            }
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::identity::{DeviceIdentity, revoke_device};

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
        assert_eq!(restarted.records(person).unwrap(), [valid].into());
    }
}
