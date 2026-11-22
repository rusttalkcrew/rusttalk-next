//! Where this device keeps its keys: the OS keychain, a protected file, or a file
//! locked with a password (for Steam Deck Game Mode and other desktops without a
//! keychain). `keystore.json` records the choice, whether an account's keys were
//! saved, and copies still to be removed. At start every place is checked, so a lost
//! or wrong `keystore.json` never hides existing keys behind a new account.

use std::{
    fs::{self, File, OpenOptions},
    io::{ErrorKind, Write},
    ops::RangeInclusive,
    path::{Path, PathBuf},
    thread::JoinHandle,
};

use anyhow::{Context, Result, anyhow, bail, ensure};
use argon2::{Algorithm, Argon2, Block, Params, Version};
use chacha20poly1305::{
    KeyInit, XChaCha20Poly1305, XNonce,
    aead::{Aead, Payload},
};
use keyring::{Entry, Error};
use rusttalk_core::identity::{DeviceCertificate, DeviceIdentity, random_bytes};
use serde::{Deserialize, Serialize};
use zeroize::Zeroizing;

const KEY_FILE: &str = "identity.key";
const STATE_FILE: &str = "keystore.json";
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
/// The costs a key file may ask for. The header is read before it is authenticated,
/// so a changed file must not be able to make unlocking hang or eat all memory.
const MEMORY_KIB: RangeInclusive<u32> = 8 * 1024..=1024 * 1024;
const PASSES: RangeInclusive<u32> = 1..=10;
const LANES: RangeInclusive<u32> = 1..=4;

/// Where the keys live.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    Keychain,
    File,
    Password,
}

impl Kind {
    fn place(self) -> Place {
        match self {
            Kind::Keychain => Place::Keychain,
            Kind::File | Kind::Password => Place::File,
        }
    }
}

/// Where a copy of the keys can be: both file kinds share `identity.key`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
enum Place {
    Keychain,
    File,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
struct State {
    storage: Option<Kind>,
    /// An account's keys were saved and not signed out, so finding none means they
    /// are missing, not that there is no account.
    #[serde(default)]
    keys_saved: bool,
    /// Places whose copy is not the profile's keys: an unfinished or old copy from a
    /// switch, or one a sign-out couldn't reach. Removed at start and sign-out.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    cleanup: Vec<Place>,
}

/// What the app shows first.
pub enum Start {
    /// The keys loaded, or there is no account yet.
    Ready(Option<Box<DeviceIdentity>>),
    /// No keychain and nothing chosen: ask where to keep the keys.
    Choose,
    /// The key file is locked with a password.
    Unlock,
    /// This profile had keys but none are found; `keychain` when they were in a
    /// keychain this session can't reach.
    Missing { keychain: bool },
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

/// A write that failed before its rename (nothing changed) or after it (the new file
/// is in place but may not have reached the disk).
#[derive(Debug)]
enum WriteError {
    Before(anyhow::Error),
    After(anyhow::Error),
}

impl std::fmt::Display for WriteError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Before(error) | Self::After(error) => write!(f, "{error:#}"),
        }
    }
}

impl std::error::Error for WriteError {}

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

impl Cost {
    fn check(self) -> Result<Params> {
        ensure!(
            MEMORY_KIB.contains(&self.memory_kib)
                && PASSES.contains(&self.passes)
                && LANES.contains(&self.lanes),
            "key file asks for unsupported password settings"
        );
        Params::new(self.memory_kib, self.passes, self.lanes, Some(32))
            .map_err(|error| anyhow!("key file settings: {error}"))
    }

    fn weaker_than(self, other: Cost) -> bool {
        self.memory_kib < other.memory_kib || self.passes < other.passes
    }
}

/// The key derived from a password, and the header that says how.
pub struct Sealer {
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
        let params = header_cost(&header).check()?;
        // Argon2's own buffer is freed unwiped; this one is zeroed on drop.
        let mut blocks = Zeroizing::new(vec![Block::default(); params.block_count()]);
        let mut key = Zeroizing::new([0; 32]);
        Argon2::new(Algorithm::Argon2id, Version::V0x13, params)
            .hash_password_into_with_memory(
                password.as_bytes(),
                &header[20..],
                key.as_mut(),
                blocks.as_mut_slice(),
            )
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

/// Password work (about half a second) on its own thread, so the window keeps drawing.
pub struct Job<T>(JoinHandle<Result<T>>);

impl<T: Send + 'static> Job<T> {
    fn spawn(work: impl FnOnce() -> Result<T> + Send + 'static) -> Self {
        Self(std::thread::spawn(work))
    }

    pub fn done(&self) -> bool {
        self.0.is_finished()
    }

    pub fn join(self) -> Result<T> {
        self.0
            .join()
            .map_err(|_| anyhow!("password work stopped"))?
    }
}

/// An opened password file: its key, the keys inside, and a stronger key to lock it
/// again with when it was made with weaker settings than today's.
pub struct Unlocked {
    sealer: Sealer,
    secret: Zeroizing<Vec<u8>>,
    upgrade: Option<Sealer>,
}

/// The profile's key storage, held under a file lock for the app's lifetime.
pub struct Keystore {
    dir: PathBuf,
    keychain: Box<dyn Keychain>,
    state: State,
    /// Set once the password file is unlocked or a password is chosen.
    sealer: Option<Sealer>,
    cost: Cost,
    /// Fail the next `keystore.json` write naming this storage, before or after
    /// (`true`) its rename.
    #[cfg(test)]
    fail_state: std::cell::Cell<Option<(Kind, bool)>>,
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
        // A damaged keystore.json is rebuilt from what is found.
        let state =
            crate::load_json::<Option<State>>(&dir.join(STATE_FILE)).unwrap_or_else(|error| {
                tracing::warn!(error = %format_args!("{error:#}"), "keystore.json is unreadable");
                None
            });
        // Without keystore.json, an account in this profile means its keys exist.
        let had_account =
            state.is_none() && ACCOUNT_FILES.iter().any(|name| dir.join(name).exists());
        let mut store = Self {
            dir: dir.to_owned(),
            keychain,
            state: state.unwrap_or_default(),
            sealer: None,
            cost,
            #[cfg(test)]
            fail_state: Default::default(),
            _lock: lock,
        };
        store.state.keys_saved |= had_account;
        store.remove_temp_files();
        store.retry_cleanup();
        let start = store.reconcile()?;
        Ok((store, start))
    }

    pub fn kind(&self) -> Option<Kind> {
        self.state.storage
    }

    /// Use whichever place has keys, preferring the chosen one, and repair the record.
    fn reconcile(&mut self) -> Result<Start> {
        let current = self.state.storage.map(Kind::place);
        // A copy still listed for cleanup is not this profile's keys: signed out, or
        // an old or unfinished copy from a switch.
        let state = &self.state;
        let usable =
            |place| !state.cleanup.contains(&place) || (state.keys_saved && Some(place) == current);
        let (file_usable, keychain_usable) = (usable(Place::File), usable(Place::Keychain));
        let file = if file_usable {
            self.read_key_file()?
        } else {
            None
        };
        let keychain_first = current == Some(Place::Keychain);
        // A file profile whose file has keys never asks the keychain (no unlock prompt).
        let mut keychain = Ok(());
        let mut secret = None;
        if keychain_first || file.is_none() {
            keychain = self.keychain.available();
            if keychain.is_ok() && keychain_usable {
                match self.keychain.get() {
                    Ok(found) => secret = found,
                    // A locked keychain whose prompt can't show counts as unreachable.
                    Err(error) => {
                        tracing::warn!(error = %format_args!("{error:#}"), "reading the keychain failed");
                        keychain = Err(error);
                    }
                }
            }
        }
        if let (Some(file), Some(secret)) = (&file, &secret)
            && plain(file).is_ok_and(|plain| plain != &secret[..])
        {
            tracing::warn!(
                "the keychain and the key file hold different keys; using the chosen one"
            );
        }
        let (kind, start) = match (file, secret) {
            (_, Some(secret)) if keychain_first => {
                (Kind::Keychain, Start::Ready(Some(Box::new(load(&secret)?))))
            }
            (Some(file), _) if file.starts_with(SEALED) => (Kind::Password, Start::Unlock),
            (Some(file), _) => (
                Kind::File,
                Start::Ready(Some(Box::new(load(plain(&file)?)?))),
            ),
            (None, Some(secret)) => (Kind::Keychain, Start::Ready(Some(Box::new(load(&secret)?)))),
            (None, None) => return Ok(self.no_keys(keychain)),
        };
        if self.state.storage != Some(kind) || !self.state.keys_saved {
            if self.state.storage.is_some_and(|chosen| chosen != kind) {
                tracing::warn!(chosen = ?self.state.storage, found = ?kind, "keys found in another place than recorded");
            }
            let repaired = self.record(|state| {
                state.storage = Some(kind);
                state.keys_saved = true;
            });
            if let Err(error) = repaired {
                tracing::warn!(error = %format_args!("{error:#}"), "repairing keystore.json failed");
                self.state.storage = Some(kind);
            }
        }
        Ok(start)
    }

    fn no_keys(&mut self, keychain: Result<()>) -> Start {
        if self.state.keys_saved {
            let in_keychain = self.state.storage.is_none_or(|kind| kind == Kind::Keychain);
            let gone = in_keychain && keychain.is_err();
            tracing::warn!(
                keychain_unreachable = gone,
                "this profile's keys are missing"
            );
            return Start::Missing { keychain: gone };
        }
        match (self.state.storage, keychain) {
            // Signed out: a new account goes to the same file.
            (Some(Kind::File), _) => Start::Ready(None),
            (Some(Kind::Keychain) | None, Ok(())) => {
                if self.state.storage.is_none()
                    && let Err(error) = self.record(|state| state.storage = Some(Kind::Keychain))
                {
                    tracing::warn!(error = %format_args!("{error:#}"), "recording key storage failed");
                    self.state.storage = Some(Kind::Keychain);
                }
                Start::Ready(None)
            }
            // A password file needs a new password first.
            (Some(Kind::Password), _) => {
                self.state.storage = None;
                Start::Choose
            }
            (_, Err(error)) => {
                tracing::info!(error = %format_args!("{error:#}"), "no keychain, asking where to keep keys");
                self.state.storage = None;
                Start::Choose
            }
        }
    }

    /// Pick where a new account's keys go (first start without a keychain). A
    /// password needs a key from [`Keystore::new_password`].
    pub fn choose(&mut self, kind: Kind, sealer: Option<Sealer>) -> Result<()> {
        self.usable(kind, sealer.as_ref())?;
        self.record(|state| {
            state.storage = Some(kind);
            state.keys_saved = false;
        })?;
        tracing::info!(?kind, "key storage chosen");
        self.sealer = sealer;
        Ok(())
    }

    /// Derive the key for a new password, away from the window's thread.
    pub fn new_password(&self, password: Zeroizing<String>) -> Job<Sealer> {
        let cost = self.cost;
        Job::spawn(move || {
            ensure!(!password.is_empty(), "empty password");
            Sealer::new(&password, cost)
        })
    }

    /// Open the password file away from the window's thread; finish with
    /// [`Keystore::finish_unlock`]. Fails with [`WrongPassword`] for a wrong password.
    pub fn start_unlock(&self, password: Zeroizing<String>) -> Result<Job<Unlocked>> {
        let file = self.read_key_file()?.context("key file is missing")?;
        let cost = self.cost;
        Ok(Job::spawn(move || {
            let (sealer, secret) = Sealer::open(&password, &file)?;
            let upgrade = header_cost(&sealer.header)
                .weaker_than(cost)
                .then(|| Sealer::new(&password, cost))
                .transpose()?;
            Ok(Unlocked {
                sealer,
                secret,
                upgrade,
            })
        }))
    }

    pub fn finish_unlock(&mut self, unlocked: Unlocked) -> Result<DeviceIdentity> {
        let identity = load(&unlocked.secret)?;
        self.sealer = Some(unlocked.sealer);
        if let Some(upgrade) = unlocked.upgrade {
            match self.write_copy(Kind::Password, Some(&upgrade), &unlocked.secret) {
                Ok(()) => {
                    tracing::info!("key file locked again with today's password settings");
                    self.sealer = Some(upgrade);
                }
                Err(error) => {
                    tracing::warn!(error = %format_args!("{error:#}"), "locking the key file with stronger settings failed");
                    if matches!(error, WriteError::After(_)) {
                        self.sealer = Some(upgrade);
                    }
                }
            }
        }
        Ok(identity)
    }

    pub fn save(&mut self, identity: &DeviceIdentity) -> Result<()> {
        let kind = self.state.storage.context("no key storage chosen")?;
        self.write_copy(kind, self.sealer.as_ref(), &encode(identity))?;
        self.record(|state| {
            state.keys_saved = true;
            state.cleanup.retain(|place| *place != kind.place());
        })?;
        Ok(())
    }

    /// Sign-out: remove every copy (keychain, key file, temporary files), whatever
    /// the current choice. A keychain copy that can't be removed now is retried.
    pub fn delete(&mut self) -> Result<()> {
        let mut cleanup = self.state.cleanup.clone();
        cleanup.retain(|place| *place != Place::Keychain);
        if let Err(error) = self.keychain.delete() {
            tracing::warn!(error = %format_args!("{error:#}"), "removing the keychain copy at sign-out failed; trying again at the next start");
            cleanup.push(Place::Keychain);
        }
        self.remove_temp_files();
        remove(&self.dir.join(KEY_FILE))?;
        self.sync()?;
        cleanup.retain(|place| *place != Place::File);
        self.record(|state| {
            state.keys_saved = false;
            state.cleanup = cleanup;
        })?;
        Ok(())
    }

    /// Move the keys to `kind` (or change the password, with a key from
    /// [`Keystore::new_password`]). Between the keychain and the file it marks the new
    /// copy unfinished, writes and reads it back, records the new choice with the old
    /// copy to remove, and then removes it, so a crash at any point loses no keys.
    pub fn switch(
        &mut self,
        identity: &DeviceIdentity,
        kind: Kind,
        sealer: Option<Sealer>,
    ) -> Result<()> {
        let old = self.state.storage.context("no key storage chosen")?;
        self.usable(kind, sealer.as_ref())?;
        let secret = encode(identity);
        let (from, to) = (old.place(), kind.place());
        if from == to {
            // The key file is replaced in one step and says its own format.
            if let Err(WriteError::Before(error)) = self.write_copy(kind, sealer.as_ref(), &secret)
            {
                return Err(error);
            }
            self.sealer = sealer;
            let recorded = self.record(|state| state.storage = Some(kind));
            self.state.storage = Some(kind);
            recorded?;
            tracing::info!(?old, new = ?kind, "key storage switched");
            return Ok(());
        }
        self.record(|state| {
            if !state.cleanup.contains(&to) {
                state.cleanup.push(to);
            }
        })?;
        if let Err(error) = self.write_copy(kind, sealer.as_ref(), &secret) {
            self.roll_back(to);
            return Err(error.into());
        }
        let back = self.read(kind, sealer.as_ref());
        if !matches!(&back, Ok(Some(back)) if **back == *secret) {
            self.roll_back(to);
            return Err(back
                .err()
                .unwrap_or_else(|| anyhow!("the new key storage gave back different keys")));
        }
        let switched = self.record(|state| {
            state.storage = Some(kind);
            state.keys_saved = true;
            state.cleanup.retain(|place| *place != to && *place != from);
            state.cleanup.push(from);
        });
        match switched {
            Err(WriteError::Before(error)) => {
                self.roll_back(to);
                return Err(error);
            }
            // The new choice may not have reached the disk: keep both copies and let
            // the next start settle it from what it finds.
            Err(error @ WriteError::After(_)) => {
                self.sealer = sealer;
                return Err(error.into());
            }
            Ok(()) => self.sealer = sealer,
        }
        match self.remove_place(from).and_then(|()| self.sync()) {
            Ok(()) => {
                if let Err(error) = self.record(|state| state.cleanup.retain(|p| *p != from)) {
                    tracing::warn!(error = %format_args!("{error:#}"), "recording the removed key copy failed");
                }
            }
            Err(error) => {
                tracing::warn!(error = %format_args!("{error:#}"), "removing the old key copy failed; trying again at the next start");
            }
        }
        tracing::info!(?old, new = ?kind, "key storage switched");
        Ok(())
    }

    fn usable(&self, kind: Kind, sealer: Option<&Sealer>) -> Result<()> {
        match kind {
            Kind::Keychain => self.keychain.available(),
            Kind::File => Ok(()),
            Kind::Password => sealer.map(|_| ()).context("no password"),
        }
    }

    /// Remove an unfinished copy after a failed switch.
    fn roll_back(&mut self, place: Place) {
        match self.remove_place(place).and_then(|()| self.sync()) {
            Ok(()) => {
                if let Err(error) = self.record(|state| state.cleanup.retain(|p| *p != place)) {
                    tracing::warn!(error = %format_args!("{error:#}"), "recording the removed key copy failed");
                }
            }
            Err(error) => {
                tracing::warn!(error = %format_args!("{error:#}"), "removing the unfinished key copy failed; trying again at the next start");
            }
        }
    }

    /// Remove the copies listed for cleanup, except the profile's own keys.
    fn retry_cleanup(&mut self) {
        if self.state.cleanup.is_empty() {
            return;
        }
        // Act only on a list that is on disk.
        if let Err(error) = self.sync() {
            tracing::warn!(error = %format_args!("{error:#}"), "flushing keystore.json failed");
            return;
        }
        let current = self.state.storage.map(Kind::place);
        let mut left = Vec::new();
        for place in self.state.cleanup.clone() {
            if self.state.keys_saved && Some(place) == current {
                continue;
            }
            if let Err(error) = self.remove_place(place) {
                tracing::warn!(error = %format_args!("{error:#}"), ?place, "removing a leftover key copy failed");
                left.push(place);
            }
        }
        if let Err(error) = self.sync() {
            tracing::warn!(error = %format_args!("{error:#}"), "flushing the profile folder failed");
            return;
        }
        if let Err(error) = self.record(|state| state.cleanup = left) {
            tracing::warn!(error = %format_args!("{error:#}"), "recording removed key copies failed");
        }
    }

    /// Temporary files left by a crash mid-write can hold keys.
    fn remove_temp_files(&self) {
        let prefixes = [KEY_FILE, STATE_FILE].map(temp_prefix);
        let Ok(entries) = fs::read_dir(&self.dir) else {
            return;
        };
        let mut removed = false;
        for entry in entries.flatten() {
            let name = entry.file_name();
            if prefixes
                .iter()
                .any(|p| name.to_string_lossy().starts_with(p.as_str()))
            {
                match fs::remove_file(entry.path()) {
                    Ok(()) => removed = true,
                    Err(error) => {
                        tracing::warn!(%error, "removing a leftover temporary key file failed");
                    }
                }
            }
        }
        if removed && let Err(error) = self.sync() {
            tracing::warn!(error = %format_args!("{error:#}"), "flushing the profile folder failed");
        }
    }

    fn remove_place(&self, place: Place) -> Result<()> {
        match place {
            Place::Keychain => self.keychain.delete(),
            Place::File => remove(&self.dir.join(KEY_FILE)),
        }
    }

    /// Write `keystore.json` with `change` applied.
    fn record(&mut self, change: impl FnOnce(&mut State)) -> Result<(), WriteError> {
        let mut next = self.state.clone();
        change(&mut next);
        #[cfg(test)]
        let fail = self
            .fail_state
            .get()
            .filter(|(kind, _)| next.storage == Some(*kind));
        #[cfg(test)]
        if let Some((_, false)) = fail {
            self.fail_state.set(None);
            return Err(WriteError::Before(anyhow!("test failure")));
        }
        let json = serde_json::to_vec(&next).map_err(|error| WriteError::Before(error.into()))?;
        write_atomic(&self.dir.join(STATE_FILE), &json).map_err(WriteError::Before)?;
        self.state = next;
        #[cfg(test)]
        if let Some((_, true)) = fail {
            self.fail_state.set(None);
            return Err(WriteError::After(anyhow!("test failure")));
        }
        self.sync().map_err(WriteError::After)
    }

    fn write_copy(
        &self,
        kind: Kind,
        sealer: Option<&Sealer>,
        secret: &[u8],
    ) -> Result<(), WriteError> {
        let file = self.dir.join(KEY_FILE);
        let written = match kind {
            Kind::Keychain => return self.keychain.set(secret).map_err(WriteError::Before),
            Kind::File => write_atomic(&file, &Zeroizing::new([PLAIN, secret].concat())),
            Kind::Password => sealer
                .context("key file is locked")
                .and_then(|sealer| write_atomic(&file, &sealer.seal(secret)?)),
        };
        written.map_err(WriteError::Before)?;
        self.sync().map_err(WriteError::After)
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
                let sealer = sealer.context("key file is locked")?;
                let rest = file
                    .strip_prefix(&sealer.header)
                    .context("key file has a different password")?;
                sealer.decrypt(rest)?
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

    /// Flush the folder, so renames and removals reach the disk.
    fn sync(&self) -> Result<()> {
        #[cfg(unix)]
        File::open(&self.dir)
            .and_then(|dir| dir.sync_all())
            .context("flush profile folder")?;
        Ok(())
    }
}

fn plain(file: &[u8]) -> Result<&[u8]> {
    match file.strip_prefix(PLAIN) {
        Some(secret) => Ok(secret),
        None => bail!("unknown key file format"),
    }
}

fn temp_prefix(name: &str) -> String {
    format!(".{name}.tmp-")
}

/// Write to a temporary file, flush it, then rename it over `path`, so a crash leaves
/// the old file or the new one (and maybe a temporary file, removed at the next
/// start). The temporary file is created 0600 on Unix; on Windows the profile folder
/// in the per-user AppData already admits only this user.
fn write_atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    let dir = path.parent().context("key folder")?;
    let name = path.file_name().context("key file name")?.to_string_lossy();
    let mut file = tempfile::Builder::new()
        .prefix(&temp_prefix(&name))
        .tempfile_in(dir)
        .context("create key file")?;
    file.write_all(bytes).context("write key file")?;
    file.as_file().sync_all().context("flush key file")?;
    file.persist(path)
        .map_err(|error| error.error)
        .with_context(|| format!("replace {}", path.display()))?;
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

    /// The cheapest settings a key file may use, so tests stay fast.
    const CHEAP: Cost = Cost {
        memory_kib: 8 * 1024,
        passes: 1,
        lanes: 1,
    };

    #[derive(Clone, Default)]
    struct Mock {
        down: bool,
        stuck: bool,
        secret: Arc<Mutex<Option<Vec<u8>>>>,
    }

    impl Mock {
        fn has_keys(&self) -> bool {
            self.secret.lock().unwrap().is_some()
        }
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
            self.available()?;
            ensure!(!self.stuck, "keychain refused");
            *self.secret.lock().unwrap() = None;
            Ok(())
        }
    }

    fn down() -> Mock {
        Mock {
            down: true,
            ..Mock::default()
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

    fn sealer(store: &Keystore, password: &str) -> Option<Sealer> {
        Some(
            store
                .new_password(Zeroizing::new(password.into()))
                .join()
                .unwrap(),
        )
    }

    fn unlock(store: &mut Keystore, password: &str) -> Result<DeviceIdentity> {
        let unlocked = store
            .start_unlock(Zeroizing::new(password.into()))?
            .join()?;
        store.finish_unlock(unlocked)
    }

    fn state(dir: &Path) -> State {
        serde_json::from_slice(&fs::read(dir.join(STATE_FILE)).unwrap()).unwrap()
    }

    fn key_file(dir: &Path) -> bool {
        dir.join(KEY_FILE).exists()
    }

    /// A profile with its keys in a protected file.
    fn file_profile(dir: &Path, keychain: &Mock) -> DeviceIdentity {
        let (mut store, _) = open(dir, keychain);
        let me = identity();
        store.choose(Kind::File, None).unwrap();
        store.save(&me).unwrap();
        me
    }

    #[test]
    fn keychain_round_trips_and_is_the_default() {
        let dir = tempfile::tempdir().unwrap();
        let keychain = Mock::default();
        let (mut store, start) = open(dir.path(), &keychain);
        assert!(ready(start).is_none());
        assert_eq!(store.kind(), Some(Kind::Keychain));
        let me = identity();
        store.save(&me).unwrap();
        drop(store);
        assert!(same(&ready(open(dir.path(), &keychain).1).unwrap(), &me));
        assert!(!key_file(dir.path()));
    }

    #[test]
    fn file_round_trips_with_owner_only_permissions() {
        let dir = tempfile::tempdir().unwrap();
        let (mut store, start) = open(dir.path(), &down());
        assert!(matches!(start, Start::Choose));
        store.choose(Kind::File, None).unwrap();
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
        assert!(same(&ready(open(dir.path(), &down()).1).unwrap(), &me));
    }

    #[test]
    fn password_file_round_trips_and_rejects_wrong_passwords_and_tampering() {
        let dir = tempfile::tempdir().unwrap();
        let (mut store, _) = open(dir.path(), &down());
        let key = sealer(&store, "hunter2");
        store.choose(Kind::Password, key).unwrap();
        let me = identity();
        store.save(&me).unwrap();
        drop(store);

        let (mut store, start) = open(dir.path(), &down());
        assert!(matches!(start, Start::Unlock));
        assert!(
            unlock(&mut store, "hunter3")
                .err()
                .unwrap()
                .is::<WrongPassword>()
        );
        assert!(same(&unlock(&mut store, "hunter2").unwrap(), &me));
        // Saving again keeps the password.
        store.save(&me).unwrap();
        drop(store);

        let path = dir.path().join(KEY_FILE);
        let mut bytes = fs::read(&path).unwrap();
        let last = bytes.len() - 1;
        bytes[last] ^= 1;
        fs::write(&path, bytes).unwrap();
        let (mut store, _) = open(dir.path(), &down());
        assert!(
            unlock(&mut store, "hunter2")
                .err()
                .unwrap()
                .is::<WrongPassword>()
        );
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
    fn header_costs_are_bounded_before_any_work() {
        let cost = |memory_kib, passes, lanes| Cost {
            memory_kib,
            passes,
            lanes,
        };
        for good in [cost(8192, 1, 1), cost(1024 * 1024, 10, 4), COST] {
            assert!(good.check().is_ok(), "{good:?}");
        }
        for bad in [
            cost(8191, 3, 1),
            cost(1024 * 1024 + 1, 3, 1),
            cost(65536, 0, 1),
            cost(65536, 11, 1),
            cost(65536, u32::MAX, 1),
            cost(65536, 3, 0),
            cost(65536, 3, 5),
        ] {
            assert!(bad.check().is_err(), "{bad:?}");
        }
        // A file asking for endless passes is refused at once, not derived.
        let mut file = Sealer::new("pw", CHEAP).unwrap().seal(b"secret").unwrap();
        file[12..16].copy_from_slice(&u32::MAX.to_be_bytes());
        let error = Sealer::open("pw", &file).err().unwrap();
        assert!(!error.is::<WrongPassword>());
    }

    #[test]
    fn weak_password_files_are_locked_again_with_todays_settings() {
        let dir = tempfile::tempdir().unwrap();
        let (mut store, _) = open(dir.path(), &down());
        let key = sealer(&store, "pw");
        store.choose(Kind::Password, key).unwrap();
        let me = identity();
        store.save(&me).unwrap();
        drop(store);
        let stronger = Cost {
            memory_kib: 16 * 1024,
            ..CHEAP
        };
        let (mut store, _) = Keystore::open_with(dir.path(), Box::new(down()), stronger).unwrap();
        assert!(same(&unlock(&mut store, "pw").unwrap(), &me));
        let file = fs::read(dir.path().join(KEY_FILE)).unwrap();
        assert_eq!(header_cost(file[..HEADER].try_into().unwrap()), stronger);
        assert!(same(&unlock(&mut store, "pw").unwrap(), &me));
    }

    #[test]
    fn switching_moves_keys_between_all_three() {
        let dir = tempfile::tempdir().unwrap();
        let keychain = Mock::default();
        let (mut store, _) = open(dir.path(), &keychain);
        let me = identity();
        store.save(&me).unwrap();

        store.switch(&me, Kind::File, None).unwrap();
        assert!(!keychain.has_keys());
        let one = sealer(&store, "one");
        store.switch(&me, Kind::Password, one).unwrap();
        let two = sealer(&store, "two");
        store.switch(&me, Kind::Password, two).unwrap();
        drop(store);
        let (mut store, start) = open(dir.path(), &keychain);
        assert!(matches!(start, Start::Unlock));
        assert!(unlock(&mut store, "one").is_err());
        unlock(&mut store, "two").unwrap();
        store.switch(&me, Kind::Keychain, None).unwrap();
        assert!(!key_file(dir.path()));
        assert!(state(dir.path()).cleanup.is_empty());
        drop(store);
        assert!(same(&ready(open(dir.path(), &keychain).1).unwrap(), &me));
    }

    #[test]
    fn a_switch_failing_before_the_choice_is_recorded_keeps_the_old_keys() {
        let dir = tempfile::tempdir().unwrap();
        let keychain = Mock::default();
        let (mut store, _) = open(dir.path(), &keychain);
        let me = identity();
        store.save(&me).unwrap();
        store.fail_state.set(Some((Kind::File, false)));
        assert!(store.switch(&me, Kind::File, None).is_err());
        assert!(keychain.has_keys());
        assert!(!key_file(dir.path()));
        assert_eq!(store.kind(), Some(Kind::Keychain));
        drop(store);
        assert!(same(&ready(open(dir.path(), &keychain).1).unwrap(), &me));
    }

    #[test]
    fn a_switch_failing_after_the_choice_rename_keeps_both_copies() {
        let dir = tempfile::tempdir().unwrap();
        let keychain = Mock::default();
        let (mut store, _) = open(dir.path(), &keychain);
        let me = identity();
        store.save(&me).unwrap();
        store.fail_state.set(Some((Kind::File, true)));
        assert!(store.switch(&me, Kind::File, None).is_err());
        assert!(keychain.has_keys());
        assert!(key_file(dir.path()));
        drop(store);
        // The next start finds the new choice and removes the old copy.
        let (store, start) = open(dir.path(), &keychain);
        assert!(same(&ready(start).unwrap(), &me));
        assert_eq!(store.kind(), Some(Kind::File));
        assert!(!keychain.has_keys());
    }

    #[test]
    fn crashes_mid_switch_leave_no_extra_copies() {
        let keychain = Mock::default();
        let me = identity();
        // Keychain to file, crashed after writing the file but before the new choice.
        let dir = tempfile::tempdir().unwrap();
        keychain.set(&encode(&me)).unwrap();
        fs::write(
            dir.path().join(KEY_FILE),
            [PLAIN, &encode(&me)[..]].concat(),
        )
        .unwrap();
        let unfinished = r#"{"storage":"keychain","keys_saved":true,"cleanup":["file"]}"#;
        fs::write(dir.path().join(STATE_FILE), unfinished).unwrap();
        fs::write(dir.path().join(".identity.key.tmp-abc"), b"plain keys").unwrap();
        let (store, start) = open(dir.path(), &keychain);
        assert!(same(&ready(start).unwrap(), &me));
        assert_eq!(store.kind(), Some(Kind::Keychain));
        assert!(!key_file(dir.path()));
        assert!(!dir.path().join(".identity.key.tmp-abc").exists());
        assert!(state(dir.path()).cleanup.is_empty());
        drop(store);

        // Keychain to password file, crashed before the keychain copy was removed.
        let dir = tempfile::tempdir().unwrap();
        let stuck = Mock {
            stuck: true,
            ..keychain.clone()
        };
        let (mut store, _) = open(dir.path(), &stuck);
        let key = sealer(&store, "pw");
        store.switch(&me, Kind::Password, key).unwrap();
        assert!(keychain.has_keys());
        assert_eq!(state(dir.path()).cleanup, [Place::Keychain]);
        drop(store);
        let (_, start) = open(dir.path(), &keychain);
        assert!(matches!(start, Start::Unlock));
        assert!(!keychain.has_keys());
        assert!(state(dir.path()).cleanup.is_empty());
    }

    #[test]
    fn sign_out_removes_every_copy_and_retries_the_keychain() {
        let dir = tempfile::tempdir().unwrap();
        let keychain = Mock::default();
        file_profile(dir.path(), &keychain);
        // An older keychain copy the current choice doesn't use.
        keychain.set(b"old copy").unwrap();
        let stuck = Mock {
            stuck: true,
            ..keychain.clone()
        };
        let (mut store, _) = open(dir.path(), &stuck);
        fs::write(dir.path().join(".keystore.json.tmp-x"), b"{}").unwrap();
        store.delete().unwrap();
        assert!(!key_file(dir.path()));
        assert!(!dir.path().join(".keystore.json.tmp-x").exists());
        assert_eq!(state(dir.path()).cleanup, [Place::Keychain]);
        drop(store);
        // The keychain works again: the next start removes the copy.
        let (store, start) = open(dir.path(), &keychain);
        assert!(ready(start).is_none());
        assert!(!keychain.has_keys());
        assert_eq!(store.kind(), Some(Kind::File));
    }

    #[test]
    fn a_signed_out_keychain_copy_that_could_not_be_removed_stays_signed_out() {
        let dir = tempfile::tempdir().unwrap();
        let keychain = Mock::default();
        let (mut store, _) = open(dir.path(), &keychain);
        store.save(&identity()).unwrap();
        drop(store);
        let stuck = Mock {
            stuck: true,
            ..keychain.clone()
        };
        let (mut store, _) = open(dir.path(), &stuck);
        store.delete().unwrap();
        drop(store);
        assert!(stuck.has_keys());
        let (_, start) = open(dir.path(), &stuck);
        assert!(ready(start).is_none());
    }

    #[test]
    fn a_failing_keychain_never_blocks_a_file_profile() {
        #[derive(Clone)]
        struct Locked;
        impl Keychain for Locked {
            fn available(&self) -> Result<()> {
                Ok(())
            }
            fn get(&self) -> Result<Option<Zeroizing<Vec<u8>>>> {
                bail!("collection is locked")
            }
            fn set(&self, _: &[u8]) -> Result<()> {
                bail!("collection is locked")
            }
            fn delete(&self) -> Result<()> {
                bail!("collection is locked")
            }
        }
        let dir = tempfile::tempdir().unwrap();
        let me = file_profile(dir.path(), &down());
        let (_, start) = Keystore::open_with(dir.path(), Box::new(Locked), CHEAP).unwrap();
        assert!(same(&ready(start).unwrap(), &me));
        // Even with keystore.json gone or damaged.
        fs::write(dir.path().join(STATE_FILE), "{broken").unwrap();
        let (store, start) = Keystore::open_with(dir.path(), Box::new(Locked), CHEAP).unwrap();
        assert!(same(&ready(start).unwrap(), &me));
        assert_eq!(store.kind(), Some(Kind::File));
        drop(store);
        // A keychain profile whose keychain can't be read shows the recovery screen.
        let dir = tempfile::tempdir().unwrap();
        let (mut store, _) = open(dir.path(), &Mock::default());
        store.save(&identity()).unwrap();
        drop(store);
        let (_, start) = Keystore::open_with(dir.path(), Box::new(Locked), CHEAP).unwrap();
        assert!(matches!(start, Start::Missing { keychain: true }));
    }

    #[test]
    fn keys_are_found_whatever_keystore_json_says() {
        let keychain = Mock::default();
        for broken in [None, Some("null"), Some(r#"{"storage":"keychain"}"#)] {
            let dir = tempfile::tempdir().unwrap();
            let me = file_profile(dir.path(), &keychain);
            match broken {
                None => fs::remove_file(dir.path().join(STATE_FILE)).unwrap(),
                Some(json) => fs::write(dir.path().join(STATE_FILE), json).unwrap(),
            }
            let (store, start) = open(dir.path(), &keychain);
            assert!(same(&ready(start).unwrap(), &me), "{broken:?}");
            assert_eq!(store.kind(), Some(Kind::File));
            assert_eq!(state(dir.path()).storage, Some(Kind::File));
            assert!(state(dir.path()).keys_saved);
        }
        // Keychain keys with the choice changed to a file.
        let dir = tempfile::tempdir().unwrap();
        let keychain = Mock::default();
        let (mut store, _) = open(dir.path(), &keychain);
        let me = identity();
        store.save(&me).unwrap();
        drop(store);
        fs::write(dir.path().join(STATE_FILE), r#"{"storage":"file"}"#).unwrap();
        let (store, start) = open(dir.path(), &keychain);
        assert!(same(&ready(start).unwrap(), &me));
        assert_eq!(store.kind(), Some(Kind::Keychain));
    }

    #[test]
    fn missing_keys_never_start_a_new_account() {
        // The keychain with the keys is unreachable.
        let dir = tempfile::tempdir().unwrap();
        let keychain = Mock::default();
        let (mut store, _) = open(dir.path(), &keychain);
        store.save(&identity()).unwrap();
        drop(store);
        let (mut store, start) = open(dir.path(), &down());
        assert!(matches!(start, Start::Missing { keychain: true }));
        assert!(store.save(&identity()).is_err());
        assert!(!key_file(dir.path()));
        drop(store);

        // The key file is gone.
        let keychain = Mock::default();
        let dir = tempfile::tempdir().unwrap();
        file_profile(dir.path(), &keychain);
        fs::remove_file(dir.path().join(KEY_FILE)).unwrap();
        let (_, start) = open(dir.path(), &keychain);
        assert!(matches!(start, Start::Missing { keychain: false }));

        // A profile from before keystore.json that has an account.
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("servers.json"), "[]").unwrap();
        assert!(matches!(
            open(dir.path(), &down()).1,
            Start::Missing { keychain: true }
        ));

        // A signed-out profile starts fresh.
        let dir = tempfile::tempdir().unwrap();
        file_profile(dir.path(), &keychain);
        let (mut store, _) = open(dir.path(), &keychain);
        store.delete().unwrap();
        drop(store);
        assert!(ready(open(dir.path(), &keychain).1).is_none());
    }
}
