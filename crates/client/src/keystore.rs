//! Where this device keeps its keys: the OS keychain, a protected file, or a file
//! locked with a password (for Steam Deck Game Mode and other desktops without a
//! keychain). `keystore.json` records the choice, so a missing keychain never
//! silently turns into a new account.

use std::{
    fs::{self, File, OpenOptions},
    io::{ErrorKind, Write},
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, anyhow, bail, ensure};
use argon2::{Algorithm, Argon2, Params, Version};
use chacha20poly1305::{
    KeyInit, XChaCha20Poly1305, XNonce,
    aead::{Aead, Payload},
};
use keyring::{Entry, Error};
use rusttalk_core::identity::{DeviceCertificate, DeviceIdentity, random_bytes};
use serde::{Deserialize, Serialize};
use zeroize::Zeroizing;

const KEY_FILE: &str = "identity.key";
const CHOICE_FILE: &str = "keystore.json";
/// Profiles from before `keystore.json` kept their keys in the keychain; these files
/// show such a profile has an account.
const ACCOUNT_FILES: [&str; 2] = ["servers.json", "sync.json"];
/// Each key file starts with its format, so the file alone says how to read it.
const PLAIN: &[u8; 8] = b"RTKEYP01";
const SEALED: &[u8; 8] = b"RTKEYE01";
/// Magic, the three Argon2 costs and the salt; authenticated as associated data.
const HEADER: usize = 8 + 12 + 16;
const NONCE: usize = 24;
/// Argon2id with 64 MiB, 3 passes and 1 lane: about half a second on a Steam Deck.
const COST: Cost = Cost {
    memory_kib: 64 * 1024,
    passes: 3,
    lanes: 1,
};

/// Where the keys live.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    Keychain,
    File,
    Password,
}

#[derive(Serialize, Deserialize)]
struct Choice {
    storage: Kind,
}

/// What the app shows first.
pub enum Start {
    /// The keys loaded, or there is no account yet.
    Ready(Option<Box<DeviceIdentity>>),
    /// No keychain and nothing chosen: ask where to keep the keys.
    Choose,
    /// The key file is locked with a password.
    Unlock,
    /// The keys are in a keychain this session can't reach.
    KeychainGone,
}

/// The password didn't open the key file (or the file was changed).
#[derive(Debug)]
pub struct WrongPassword;

impl std::fmt::Display for WrongPassword {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("wrong password or damaged key file")
    }
}

impl std::error::Error for WrongPassword {}

/// The OS keychain, behind a seam so tests never touch the real one.
pub trait Keychain {
    /// Fails when this session has no reachable keychain.
    fn available(&self) -> Result<()>;
    fn get(&self) -> Result<Option<Zeroizing<Vec<u8>>>>;
    fn set(&self, secret: &[u8]) -> Result<()>;
    /// Removing a missing entry succeeds.
    fn delete(&self) -> Result<()>;
}

struct OsKeychain(String);

impl OsKeychain {
    fn entry(&self) -> Result<Entry> {
        self.available()?;
        Entry::new(&self.0, "identity-v1").context("open OS keychain entry")
    }
}

impl Keychain for OsKeychain {
    fn available(&self) -> Result<()> {
        Entry::store_status()
            .as_ref()
            .map_err(|error| anyhow!("open OS keychain: {error}"))?;
        Ok(())
    }

    fn get(&self) -> Result<Option<Zeroizing<Vec<u8>>>> {
        match self.entry()?.get_secret() {
            Ok(secret) => Ok(Some(Zeroizing::new(secret))),
            Err(Error::NoEntry) => Ok(None),
            Err(error) => Err(error).context("read identity from OS keychain"),
        }
    }

    fn set(&self, secret: &[u8]) -> Result<()> {
        self.entry()?
            .set_secret(secret)
            .context("save identity to OS keychain")
    }

    fn delete(&self) -> Result<()> {
        match self.entry()?.delete_credential() {
            Ok(()) | Err(Error::NoEntry) => Ok(()),
            Err(error) => Err(error).context("remove identity from OS keychain"),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Cost {
    memory_kib: u32,
    passes: u32,
    lanes: u32,
}

/// The key derived from a password, and the header that says how.
struct Sealer {
    key: Zeroizing<[u8; 32]>,
    header: [u8; HEADER],
}

impl Sealer {
    fn new(password: &str, cost: Cost) -> Result<Self> {
        let mut salt = [0; 16];
        random_bytes(&mut salt)?;
        let mut header = [0; HEADER];
        header[..8].copy_from_slice(SEALED);
        header[8..12].copy_from_slice(&cost.memory_kib.to_be_bytes());
        header[12..16].copy_from_slice(&cost.passes.to_be_bytes());
        header[16..20].copy_from_slice(&cost.lanes.to_be_bytes());
        header[20..].copy_from_slice(&salt);
        Self::derive(password, header)
    }

    fn derive(password: &str, header: [u8; HEADER]) -> Result<Self> {
        let cost = header_cost(&header);
        let params = Params::new(cost.memory_kib, cost.passes, cost.lanes, Some(32))
            .map_err(|error| anyhow!("key file settings: {error}"))?;
        let mut key = Zeroizing::new([0; 32]);
        Argon2::new(Algorithm::Argon2id, Version::V0x13, params)
            .hash_password_into(password.as_bytes(), &header[20..], key.as_mut())
            .map_err(|error| anyhow!("derive key from password: {error}"))?;
        Ok(Self { key, header })
    }

    /// Derive the key with the costs and salt in `file`'s header, then open it.
    fn open(password: &str, file: &[u8]) -> Result<(Self, Zeroizing<Vec<u8>>)> {
        let (header, rest) = file
            .split_first_chunk::<HEADER>()
            .context("truncated key file")?;
        ensure!(header.starts_with(SEALED), "key file has no password");
        let sealer = Self::derive(password, *header)?;
        let secret = sealer.decrypt(rest)?;
        Ok((sealer, secret))
    }

    fn decrypt(&self, rest: &[u8]) -> Result<Zeroizing<Vec<u8>>> {
        let (nonce, msg) = rest
            .split_first_chunk::<NONCE>()
            .context("truncated key file")?;
        let payload = Payload {
            msg,
            aad: &self.header,
        };
        XChaCha20Poly1305::new(&(*self.key).into())
            .decrypt(&XNonce::from(*nonce), payload)
            .map(Zeroizing::new)
            .map_err(|_| WrongPassword.into())
    }

    fn seal(&self, secret: &[u8]) -> Result<Vec<u8>> {
        let mut nonce = [0; NONCE];
        random_bytes(&mut nonce)?;
        let payload = Payload {
            msg: secret,
            aad: &self.header,
        };
        let sealed = XChaCha20Poly1305::new(&(*self.key).into())
            .encrypt(&nonce.into(), payload)
            .map_err(|_| anyhow!("lock key file"))?;
        Ok([&self.header[..], &nonce, &sealed].concat())
    }
}

fn header_cost(header: &[u8; HEADER]) -> Cost {
    let word = |at: usize| u32::from_be_bytes(header[at..at + 4].try_into().expect("4 bytes"));
    Cost {
        memory_kib: word(8),
        passes: word(12),
        lanes: word(16),
    }
}

/// The profile's key storage, held under a file lock for the app's lifetime.
pub struct Keystore {
    dir: PathBuf,
    keychain: Box<dyn Keychain>,
    kind: Option<Kind>,
    /// Set once the password file is unlocked or a password is chosen.
    sealer: Option<Sealer>,
    cost: Cost,
    _lock: File,
}

impl Keystore {
    /// Lock the profile and find out what to show first.
    pub fn open(config: &Path, service: &str) -> Result<(Self, Start)> {
        Self::open_with(config, Box::new(OsKeychain(service.to_owned())), COST)
    }

    fn open_with(dir: &Path, keychain: Box<dyn Keychain>, cost: Cost) -> Result<(Self, Start)> {
        let lock = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(dir.join("identity.lock"))
            .context("open identity lock")?;
        lock.try_lock()
            .context("RustTalk is already open with this profile")?;
        let chosen = crate::load_json::<Option<Choice>>(&dir.join(CHOICE_FILE))?
            .map(|choice| choice.storage);
        let mut store = Self {
            dir: dir.to_owned(),
            keychain,
            kind: chosen,
            sealer: None,
            cost,
            _lock: lock,
        };
        let start = match chosen {
            // The file says which of the two it is, even after a switch cut short.
            Some(Kind::File | Kind::Password) => match store.read_key_file()? {
                // Signed out: a new account goes to the same file.
                None if chosen == Some(Kind::File) => Start::Ready(None),
                // A new password is needed first.
                None => {
                    store.kind = None;
                    Start::Choose
                }
                Some(file) if file.starts_with(SEALED) => {
                    store.kind = Some(Kind::Password);
                    Start::Unlock
                }
                Some(file) => {
                    store.kind = Some(Kind::File);
                    Start::Ready(Some(Box::new(load(plain(&file)?)?)))
                }
            },
            Some(Kind::Keychain) | None => match store.keychain.available() {
                Ok(()) => {
                    if chosen.is_none() {
                        write_choice(dir, Kind::Keychain)?;
                        store.kind = Some(Kind::Keychain);
                    }
                    let secret = store.keychain.get()?;
                    Start::Ready(
                        secret
                            .map(|secret| load(&secret).map(Box::new))
                            .transpose()?,
                    )
                }
                Err(error)
                    if chosen.is_some() || ACCOUNT_FILES.iter().any(|f| dir.join(f).exists()) =>
                {
                    tracing::warn!(error = %format_args!("{error:#}"), "the keychain with this profile's keys is unavailable");
                    store.kind = Some(Kind::Keychain);
                    Start::KeychainGone
                }
                Err(error) => {
                    tracing::info!(error = %format_args!("{error:#}"), "no keychain, asking where to keep keys");
                    Start::Choose
                }
            },
        };
        Ok((store, start))
    }

    pub fn kind(&self) -> Option<Kind> {
        self.kind
    }

    /// Pick where a new account's keys go (first start without a keychain).
    pub fn choose(&mut self, kind: Kind, password: &str) -> Result<()> {
        let sealer = self.prepare(kind, password)?;
        write_choice(&self.dir, kind)?;
        tracing::info!(?kind, "key storage chosen");
        self.kind = Some(kind);
        self.sealer = sealer;
        Ok(())
    }

    /// Open the password file. Fails with [`WrongPassword`] for a wrong password.
    pub fn unlock(&mut self, password: &str) -> Result<DeviceIdentity> {
        let file = self.read_key_file()?.context("key file is missing")?;
        let (sealer, secret) = Sealer::open(password, &file)?;
        let identity = load(&secret)?;
        self.sealer = Some(sealer);
        Ok(identity)
    }

    pub fn save(&self, identity: &DeviceIdentity) -> Result<()> {
        let kind = self.kind.context("no key storage chosen")?;
        self.write(kind, self.sealer.as_ref(), &encode(identity))
    }

    /// Sign-out: remove the keys from the keychain and the key file.
    pub fn delete(&self) -> Result<()> {
        if self.kind == Some(Kind::Keychain) {
            self.keychain.delete()?;
        }
        remove(&self.dir.join(KEY_FILE))
    }

    /// Move the keys to `kind` (or change the password): write the new copy, read it
    /// back, record the choice, and only then remove the old copy.
    pub fn switch(&mut self, identity: &DeviceIdentity, kind: Kind, password: &str) -> Result<()> {
        let old = self.kind.context("no key storage chosen")?;
        let sealer = self.prepare(kind, password)?;
        let secret = encode(identity);
        let moved = (old == Kind::Keychain) != (kind == Kind::Keychain);
        self.write(kind, sealer.as_ref(), &secret)?;
        let checked = self.read(kind, sealer.as_ref()).and_then(|back| {
            ensure!(
                back.is_some_and(|back| *back == *secret),
                "the new key storage gave back different keys"
            );
            write_choice(&self.dir, kind)
        });
        if moved {
            // Remove the half-made copy on failure, the old one on success.
            let (target, what) = match checked {
                Err(_) => (kind, "unfinished"),
                Ok(()) => (old, "old"),
            };
            let removed = match target {
                Kind::Keychain => self.keychain.delete(),
                _ => remove(&self.dir.join(KEY_FILE)),
            };
            if let Err(error) = removed {
                tracing::warn!(error = %format_args!("{error:#}"), copy = what, "removing a key copy failed");
            }
        }
        // A replaced key file says its own format, so it is current even if recording
        // the choice failed.
        if checked.is_ok() || !moved {
            self.kind = Some(kind);
            self.sealer = sealer;
        }
        checked?;
        tracing::info!(?old, new = ?kind, "key storage switched");
        Ok(())
    }

    /// Check that `kind` can be used, deriving the key for a password.
    fn prepare(&self, kind: Kind, password: &str) -> Result<Option<Sealer>> {
        match kind {
            Kind::Keychain => self.keychain.available().map(|()| None),
            Kind::File => Ok(None),
            Kind::Password => {
                ensure!(!password.is_empty(), "empty password");
                Sealer::new(password, self.cost).map(Some)
            }
        }
    }

    fn write(&self, kind: Kind, sealer: Option<&Sealer>, secret: &[u8]) -> Result<()> {
        let file = self.dir.join(KEY_FILE);
        match kind {
            Kind::Keychain => self.keychain.set(secret),
            Kind::File => write_atomic(&file, &Zeroizing::new([PLAIN, secret].concat())),
            Kind::Password => {
                let sealer = sealer.context("key file is locked")?;
                write_atomic(&file, &sealer.seal(secret)?)
            }
        }
    }

    fn read(&self, kind: Kind, sealer: Option<&Sealer>) -> Result<Option<Zeroizing<Vec<u8>>>> {
        if kind == Kind::Keychain {
            return self.keychain.get();
        }
        let Some(file) = self.read_key_file()? else {
            return Ok(None);
        };
        Ok(Some(match kind {
            Kind::Password => {
                let rest = file
                    .strip_prefix(&sealer.context("key file is locked")?.header)
                    .context("key file has a different password")?;
                sealer.expect("checked above").decrypt(rest)?
            }
            _ => Zeroizing::new(plain(&file)?.to_vec()),
        }))
    }

    fn read_key_file(&self) -> Result<Option<Zeroizing<Vec<u8>>>> {
        match fs::read(self.dir.join(KEY_FILE)) {
            Ok(bytes) => Ok(Some(Zeroizing::new(bytes))),
            Err(error) if error.kind() == ErrorKind::NotFound => Ok(None),
            Err(error) => Err(error).context("read key file"),
        }
    }
}

fn plain(file: &[u8]) -> Result<&[u8]> {
    match file.strip_prefix(PLAIN) {
        Some(secret) => Ok(secret),
        None => bail!("unknown key file format"),
    }
}

fn write_choice(dir: &Path, kind: Kind) -> Result<()> {
    let json = serde_json::to_vec(&Choice { storage: kind })?;
    write_atomic(&dir.join(CHOICE_FILE), &json)
}

/// Write to a temporary file, flush it to disk, then rename it over `path`, so a crash
/// leaves the old file or the new one. The temporary file is created 0600 on Unix; on
/// Windows the profile folder in the per-user AppData already admits only this user.
fn write_atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    let dir = path.parent().context("key folder")?;
    let mut file = tempfile::NamedTempFile::new_in(dir).context("create key file")?;
    file.write_all(bytes).context("write key file")?;
    file.as_file().sync_all().context("flush key file")?;
    file.persist(path)
        .map_err(|error| error.error)
        .with_context(|| format!("replace {}", path.display()))?;
    #[cfg(unix)]
    File::open(dir)
        .and_then(|dir| dir.sync_all())
        .context("flush key folder")?;
    Ok(())
}

fn remove(path: &Path) -> Result<()> {
    match fs::remove_file(path) {
        Err(error) if error.kind() != ErrorKind::NotFound => {
            Err(error).with_context(|| format!("remove {}", path.display()))
        }
        _ => Ok(()),
    }
}

/// The 128 bytes of keys, then the certificate.
fn encode(identity: &DeviceIdentity) -> Zeroizing<Vec<u8>> {
    let certificate = identity.certificate.to_bytes();
    let mut secret = Zeroizing::new(Vec::with_capacity(128 + certificate.len()));
    secret.extend_from_slice(identity.signing_key.as_bytes());
    secret.extend_from_slice(identity.agreement_key.as_bytes());
    secret.extend_from_slice(identity.sync_key.as_ref());
    secret.extend_from_slice(identity.certificate.person_id());
    secret.extend_from_slice(&certificate);
    secret
}

fn load(secret: &[u8]) -> Result<DeviceIdentity> {
    let (keys, certificate) = secret
        .split_first_chunk::<128>()
        .context("truncated stored identity")?;
    let [signing, agreement, sync, person_id] = keys.as_chunks::<32>().0 else {
        unreachable!("128 bytes contains four 32-byte keys");
    };
    let certificate =
        DeviceCertificate::verify(*person_id, certificate).context("verify stored certificate")?;
    DeviceIdentity::from_key_bytes(signing, agreement, sync, certificate)
        .context("load stored device keys")
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use super::*;

    const CHEAP: Cost = Cost {
        memory_kib: 64,
        passes: 1,
        lanes: 1,
    };

    #[derive(Clone, Default)]
    struct Mock {
        down: bool,
        secret: Arc<Mutex<Option<Vec<u8>>>>,
    }

    impl Keychain for Mock {
        fn available(&self) -> Result<()> {
            ensure!(!self.down, "no keychain");
            Ok(())
        }
        fn get(&self) -> Result<Option<Zeroizing<Vec<u8>>>> {
            self.available()?;
            Ok(self.secret.lock().unwrap().clone().map(Zeroizing::new))
        }
        fn set(&self, secret: &[u8]) -> Result<()> {
            self.available()?;
            *self.secret.lock().unwrap() = Some(secret.to_vec());
            Ok(())
        }
        fn delete(&self) -> Result<()> {
            *self.secret.lock().unwrap() = None;
            Ok(())
        }
    }

    fn identity() -> DeviceIdentity {
        DeviceIdentity::create("Test").unwrap().1
    }

    fn open(dir: &Path, keychain: &Mock) -> (Keystore, Start) {
        Keystore::open_with(dir, Box::new(keychain.clone()), CHEAP).unwrap()
    }

    fn ready(start: Start) -> Option<DeviceIdentity> {
        match start {
            Start::Ready(identity) => identity.map(|identity| *identity),
            _ => panic!("expected the keys to load"),
        }
    }

    fn same(a: &DeviceIdentity, b: &DeviceIdentity) -> bool {
        *encode(a) == *encode(b)
    }

    #[test]
    fn keychain_round_trips_and_is_the_default() {
        let dir = tempfile::tempdir().unwrap();
        let keychain = Mock::default();
        let (store, start) = open(dir.path(), &keychain);
        assert!(ready(start).is_none());
        assert_eq!(store.kind(), Some(Kind::Keychain));
        let me = identity();
        store.save(&me).unwrap();
        drop(store);
        assert!(same(&ready(open(dir.path(), &keychain).1).unwrap(), &me));
        assert!(!dir.path().join(KEY_FILE).exists());
    }

    #[test]
    fn file_round_trips_with_owner_only_permissions() {
        let dir = tempfile::tempdir().unwrap();
        let keychain = Mock {
            down: true,
            ..Mock::default()
        };
        let (mut store, start) = open(dir.path(), &keychain);
        assert!(matches!(start, Start::Choose));
        store.choose(Kind::File, "").unwrap();
        let me = identity();
        store.save(&me).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(dir.path().join(KEY_FILE))
                .unwrap()
                .permissions();
            assert_eq!(mode.mode() & 0o777, 0o600);
        }
        drop(store);
        assert!(same(&ready(open(dir.path(), &keychain).1).unwrap(), &me));
    }

    #[test]
    fn password_file_round_trips_and_rejects_wrong_passwords_and_tampering() {
        let dir = tempfile::tempdir().unwrap();
        let keychain = Mock {
            down: true,
            ..Mock::default()
        };
        let (mut store, _) = open(dir.path(), &keychain);
        store.choose(Kind::Password, "hunter2").unwrap();
        let me = identity();
        store.save(&me).unwrap();
        drop(store);

        let (mut store, start) = open(dir.path(), &keychain);
        assert!(matches!(start, Start::Unlock));
        let wrong = store.unlock("hunter3").err().unwrap();
        assert!(wrong.is::<WrongPassword>());
        assert!(same(&store.unlock("hunter2").unwrap(), &me));
        // Saving again keeps the password.
        store.save(&me).unwrap();
        drop(store);

        let path = dir.path().join(KEY_FILE);
        let mut bytes = fs::read(&path).unwrap();
        let last = bytes.len() - 1;
        bytes[last] ^= 1;
        fs::write(&path, bytes).unwrap();
        let (mut store, _) = open(dir.path(), &keychain);
        assert!(store.unlock("hunter2").err().unwrap().is::<WrongPassword>());
    }

    #[test]
    fn argon2_costs_come_from_the_header() {
        let sealer = Sealer::new("pw", CHEAP).unwrap();
        let file = sealer.seal(b"secret").unwrap();
        assert_eq!(header_cost(file[..HEADER].try_into().unwrap()), CHEAP);
        // Opening uses the header's costs, not today's defaults.
        let (_, secret) = Sealer::open("pw", &file).unwrap();
        assert_eq!(&**secret, b"secret");
        // Changed costs fail authentication.
        let mut changed = file.clone();
        changed[15] = 2;
        assert!(
            Sealer::open("pw", &changed)
                .err()
                .unwrap()
                .is::<WrongPassword>()
        );
    }

    #[test]
    fn switching_moves_keys_between_all_three() {
        let dir = tempfile::tempdir().unwrap();
        let keychain = Mock::default();
        let (mut store, _) = open(dir.path(), &keychain);
        let me = identity();
        store.save(&me).unwrap();

        store.switch(&me, Kind::File, "").unwrap();
        assert!(keychain.secret.lock().unwrap().is_none());
        store.switch(&me, Kind::Password, "one").unwrap();
        store.switch(&me, Kind::Password, "two").unwrap();
        drop(store);
        let (mut store, start) = open(dir.path(), &keychain);
        assert!(matches!(start, Start::Unlock));
        assert!(store.unlock("one").is_err());
        store.unlock("two").unwrap();
        store.switch(&me, Kind::Keychain, "").unwrap();
        assert!(!dir.path().join(KEY_FILE).exists());
        drop(store);
        assert!(same(&ready(open(dir.path(), &keychain).1).unwrap(), &me));
    }

    #[test]
    fn interrupted_switch_keeps_the_old_keys() {
        let dir = tempfile::tempdir().unwrap();
        let keychain = Mock::default();
        let (mut store, _) = open(dir.path(), &keychain);
        let me = identity();
        store.save(&me).unwrap();
        // Recording the new choice fails after the new copy was written.
        fs::remove_file(dir.path().join(CHOICE_FILE)).unwrap();
        fs::create_dir(dir.path().join(CHOICE_FILE)).unwrap();
        assert!(store.switch(&me, Kind::File, "").is_err());
        assert!(keychain.secret.lock().unwrap().is_some());
        assert!(!dir.path().join(KEY_FILE).exists());
        assert_eq!(store.kind(), Some(Kind::Keychain));
        store.save(&me).unwrap();
    }

    #[test]
    fn a_missing_keychain_never_starts_a_new_account() {
        let dir = tempfile::tempdir().unwrap();
        let keychain = Mock::default();
        let (store, _) = open(dir.path(), &keychain);
        store.save(&identity()).unwrap();
        drop(store);
        let gone = Mock {
            down: true,
            ..keychain.clone()
        };
        let (store, start) = open(dir.path(), &gone);
        assert!(matches!(start, Start::KeychainGone));
        assert!(store.save(&identity()).is_err());
        assert!(!dir.path().join(KEY_FILE).exists());

        // A profile from before keystore.json that has an account counts too.
        let old = tempfile::tempdir().unwrap();
        fs::write(old.path().join("servers.json"), "[]").unwrap();
        assert!(matches!(open(old.path(), &gone).1, Start::KeychainGone));
    }

    #[test]
    fn sign_out_removes_the_key_file() {
        let dir = tempfile::tempdir().unwrap();
        let keychain = Mock::default();
        let (mut store, _) = open(dir.path(), &keychain);
        let me = identity();
        store.save(&me).unwrap();
        store.switch(&me, Kind::File, "").unwrap();
        store.delete().unwrap();
        assert!(!dir.path().join(KEY_FILE).exists());
        drop(store);
        let (store, start) = open(dir.path(), &keychain);
        assert!(ready(start).is_none());
        assert_eq!(store.kind(), Some(Kind::File));
    }
}
