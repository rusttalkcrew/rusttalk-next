use std::{
    env,
    io::Write,
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
        mpsc::{self, Receiver, TryRecvError},
    },
    time::{Duration, Instant},
};
use std::{fs, process::Command};

use anyhow::{Context, Result, anyhow, ensure};
use ed25519_dalek::{Signature, VerifyingKey};
use eframe::egui;
use rusttalk_core::client::save_json;
use semver::Version;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tempfile::NamedTempFile;
use url::Url;

const LOCAL_TEST: bool = match option_env!("RUSTTALK_LOCAL_UPDATE_TEST") {
    Some(value) => matches!(value.as_bytes(), b"1"),
    None => false,
};

#[cfg(any(target_os = "linux", target_os = "macos"))]
static RESTART: std::sync::OnceLock<Target> = std::sync::OnceLock::new();

/// The check intervals people can pick, in minutes. The first is the default and the
/// shortest: a hand-edited smaller value is raised to it, so GitHub is never hammered.
const CHECK_MINUTES: [u64; 4] = [15, 60, 6 * 60, 24 * 60];

#[derive(Deserialize, Serialize)]
struct Settings {
    #[serde(default = "default_automatic")]
    automatic: bool,
    #[serde(default = "default_url")]
    url: String,
    /// How often a running app looks for a new version; settings saved before this
    /// existed get the default.
    #[serde(default = "default_check_minutes")]
    check_minutes: u64,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            automatic: default_automatic(),
            url: default_url(),
            check_minutes: default_check_minutes(),
        }
    }
}

fn default_check_minutes() -> u64 {
    CHECK_MINUTES[0]
}

/// The official release directory, so every build starts with a working address.
const OFFICIAL_URL: &str =
    "https://github.com/rusttalkcrew/rusttalk-next/releases/latest/download/";

/// Release builds point at their own release directory (forks set their own).
fn default_url() -> String {
    option_env!("RUSTTALK_DEFAULT_UPDATE_URL")
        .unwrap_or(OFFICIAL_URL)
        .to_owned()
}

/// Checking is on by default; a saved choice always wins over this.
fn default_automatic() -> bool {
    true
}

/// The release page for `version`, when the update directory is a GitHub release
/// download directory (`github.com/<owner>/<repo>/releases/.../download/`); other
/// hosts have no page we can name.
fn release_notes(base: &Url, version: &Version) -> Option<Url> {
    let mut parts = base.path_segments()?.filter(|part| !part.is_empty());
    let (owner, repo, releases) = (parts.next()?, parts.next()?, parts.next()?);
    if base.scheme() != "https" || base.host_str() != Some("github.com") || releases != "releases" {
        return None;
    }
    Url::parse(&format!(
        "https://github.com/{owner}/{repo}/releases/tag/v{version}"
    ))
    .ok()
}

/// Whether the scheduled check should run now.
fn check_due(next: Option<Instant>, now: Instant) -> bool {
    next.is_none_or(|next| now >= next)
}

impl Settings {
    fn check_every(&self) -> Duration {
        Duration::from_secs(self.check_minutes.max(CHECK_MINUTES[0]) * 60)
    }

    fn load(path: &Path) -> Result<Self> {
        let mut settings: Self = crate::load_json(path)?;
        // Older builds saved an empty address with checks off before there was a
        // default: that was never a choice, so start from the defaults.
        if settings.url.trim().is_empty() {
            settings.url = default_url();
            settings.automatic = default_automatic();
        }
        Ok(settings)
    }

    fn save(&self, path: &Path) -> Result<()> {
        save_json(path.parent().context("locate update settings")?, path, self)
    }
}

#[derive(Deserialize)]
struct Manifest {
    version: String,
    platforms: std::collections::HashMap<String, Artifact>,
}

#[derive(Deserialize)]
struct Artifact {
    file: String,
    size: u64,
    sha256: String,
}

impl Artifact {
    fn verify(&self, size: u64, digest: &[u8]) -> Result<()> {
        ensure!(size == self.size, "update size mismatch");
        ensure!(
            hex::decode(&self.sha256)? == digest,
            "update SHA-256 mismatch"
        );
        Ok(())
    }
}

struct Update {
    base: Url,
    version: Version,
    artifact: Artifact,
}

fn verify(key: &VerifyingKey, bytes: &[u8], signature: &[u8]) -> Result<Manifest> {
    let signature = Signature::from_slice(signature)
        .map_err(|_| anyhow!("invalid release signature length"))?;
    key.verify_strict(bytes, &signature)
        .map_err(|_| anyhow!("invalid release manifest signature"))?;
    serde_json::from_slice(bytes).context("invalid release manifest")
}

fn newer(version: &Version, running: &Version) -> Result<bool> {
    ensure!(
        !version.cmp_precedence(running).is_lt(),
        "refusing an update downgrade"
    );
    Ok(version.cmp_precedence(running).is_gt())
}

fn release_key() -> Result<VerifyingKey> {
    let mut key = [0; 32];
    let public =
        option_env!("RUSTTALK_RELEASE_PUBLIC_KEY").context("no release key in this build")?;
    hex::decode_to_slice(public, &mut key).context("invalid built-in release key")?;
    VerifyingKey::from_bytes(&key).map_err(|_| anyhow!("invalid built-in release key"))
}

/// `etag` asks the host to answer "not modified" (HTTP 304) when nothing changed.
async fn get(client: &reqwest::Client, url: Url, etag: Option<&str>) -> Result<reqwest::Response> {
    let mut request = client.get(url);
    if let Some(etag) = etag {
        request = request.header(reqwest::header::IF_NONE_MATCH, etag);
    }
    let response = request.send().await.map_err(reqwest::Error::without_url)?;
    let status = response.status();
    ensure!(
        status == reqwest::StatusCode::OK
            || (etag.is_some() && status == reqwest::StatusCode::NOT_MODIFIED),
        "update host returned HTTP {status}"
    );
    Ok(response)
}

async fn read_small(client: &reqwest::Client, url: Url, limit: usize) -> Result<Vec<u8>> {
    read_body(get(client, url, None).await?, limit).await
}

async fn read_body(mut response: reqwest::Response, limit: usize) -> Result<Vec<u8>> {
    let mut bytes = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(reqwest::Error::without_url)?
    {
        ensure!(
            chunk.len() <= limit - bytes.len(),
            "oversized release metadata"
        );
        bytes.extend_from_slice(&chunk);
    }
    Ok(bytes)
}

/// What a check found.
enum Found {
    /// The host says the manifest is the one already seen.
    Unchanged,
    /// Nothing newer; the manifest's ETag, to ask "unchanged?" next time.
    Current(Option<String>),
    Newer(Update),
}

fn update_client(local_http: bool) -> Result<reqwest::Client> {
    Ok(rusttalk_core::route::configure_reqwest(
        reqwest::Client::builder()
            .https_only(!local_http)
            // Release hosts such as GitHub redirect downloads to a CDN. The signed
            // manifest pins every byte, so only the scheme needs checking here.
            .redirect(reqwest::redirect::Policy::custom(move |attempt| {
                if attempt.previous().len() < 5 && (attempt.url().scheme() == "https" || local_http)
                {
                    attempt.follow()
                } else {
                    attempt.stop()
                }
            }))
            .connect_timeout(Duration::from_secs(10))
            .timeout(Duration::from_secs(300)),
    )?
    .build()?)
}

/// Looks for a newer release. `etag` is the ETag of the last manifest that held
/// nothing newer: GitHub then answers 304 and the 15 minute checks cost almost nothing.
async fn check(url: &str, etag: Option<&str>) -> Result<Found> {
    let key = release_key()?;
    let mut base = Url::parse(url.trim()).map_err(|_| anyhow!("enter a valid update URL"))?;
    let local_http = LOCAL_TEST && base.scheme() == "http" && base.host_str() == Some("127.0.0.1");
    ensure!(
        (base.scheme() == "https" || local_http)
            && base.host_str().is_some()
            && base.username().is_empty()
            && base.password().is_none()
            && base.query().is_none()
            && base.fragment().is_none(),
        "enter an HTTPS update directory URL without credentials, query or fragment"
    );
    base.path_segments_mut()
        .expect("HTTP URL has a path")
        .pop_if_empty()
        .push("");
    let client = update_client(local_http)?;
    let response = get(&client, base.join("latest.json")?, etag).await?;
    if response.status() == reqwest::StatusCode::NOT_MODIFIED {
        return Ok(Found::Unchanged);
    }
    let etag = response
        .headers()
        .get(reqwest::header::ETAG)
        .and_then(|etag| etag.to_str().ok())
        .map(str::to_owned);
    let bytes = read_body(response, 16 * 1024).await?;
    let signature = read_small(&client, base.join("latest.json.sig")?, 64).await?;
    let mut manifest = verify(&key, &bytes, &signature)?;
    let version = Version::parse(&manifest.version)?;
    if !newer(&version, &Version::parse(env!("CARGO_PKG_VERSION"))?)? {
        return Ok(Found::Current(etag));
    }
    let platform = format!("{}-{}", env::consts::OS, env::consts::ARCH);
    let artifact = manifest
        .platforms
        .remove(&platform)
        .context("no update for this platform")?;
    ensure!(
        !matches!(artifact.file.as_str(), "" | "." | "..")
            && artifact
                .file
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b)),
        "invalid update filename"
    );
    ensure!(
        artifact.size > 0 && artifact.size <= 1024 * 1024 * 1024,
        "invalid update size"
    );
    Ok(Found::Newer(Update {
        base,
        version,
        artifact,
    }))
}

#[derive(Clone)]
struct Target {
    path: PathBuf,
    /// The installed file (Linux) or bundle folder (macOS) as RustTalk found it.
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    inode: (u64, u64),
}

impl Target {
    fn new(config: &Path) -> Result<Self> {
        #[cfg(target_os = "linux")]
        {
            let _ = config;
            let executable = env::current_exe()?;
            if let Some(target) = running_appimage(&executable)? {
                return Ok(target);
            }
            // Test builds may replace a disposable copy of the plain executable.
            ensure!(
                LOCAL_TEST,
                "install the RustTalk AppImage to use in-app updates"
            );
            Ok(Self {
                inode: file_id(&fs::metadata(&executable)?),
                path: executable,
            })
        }
        #[cfg(target_os = "windows")]
        {
            Ok(Self {
                path: config.join("rusttalk-update.exe"),
            })
        }
        // Phones update through their app store or a new download, never from here.
        #[cfg(any(target_os = "android", target_os = "ios"))]
        {
            let _ = config;
            anyhow::bail!("this app is updated through its download page or app store")
        }
        #[cfg(target_os = "macos")]
        {
            let _ = config;
            let path = bundle_of(&fs::canonicalize(env::current_exe()?)?)
                .context("install RustTalk.app to use in-app updates")?;
            // macOS runs a quarantined app opened from Downloads from a read-only copy.
            ensure!(
                !path.to_string_lossy().contains("/AppTranslocation/"),
                "move RustTalk to Applications to use in-app updates"
            );
            Ok(Self {
                inode: file_id(&fs::symlink_metadata(&path)?),
                path,
            })
        }
    }

    async fn install(self, update: Update, done: Arc<AtomicU64>) -> Result<State> {
        // hdiutil reopens the image by path after it is verified, so it is kept in
        // this user's private cache, where no one else can swap it.
        #[cfg(target_os = "macos")]
        let cache = cache_dir()?;
        #[cfg(target_os = "macos")]
        let download = tempfile::Builder::new()
            .prefix("download")
            .tempdir_in(&cache)?;
        #[cfg(target_os = "macos")]
        let mut file = NamedTempFile::new_in(download.path())?;
        #[cfg(not(target_os = "macos"))]
        let mut file =
            NamedTempFile::new_in(self.path.parent().context("locate update directory")?)?;
        // Always build a fresh routed HTTP client at install time so a route switch or
        // route block after the check cannot download through an old direct client.
        let local_http = LOCAL_TEST
            && update.base.scheme() == "http"
            && update.base.host_str() == Some("127.0.0.1");
        let client = update_client(local_http)?;
        let mut response = get(&client, update.base.join(&update.artifact.file)?, None).await?;
        let mut size = 0;
        let mut hasher = Sha256::new();
        while let Some(chunk) = response
            .chunk()
            .await
            .map_err(reqwest::Error::without_url)?
        {
            size += chunk.len() as u64;
            ensure!(size <= update.artifact.size, "update size mismatch");
            hasher.update(&chunk);
            file.write_all(&chunk)?;
            done.store(size, Ordering::Relaxed);
        }
        update.artifact.verify(size, &hasher.finalize())?;
        #[cfg(target_os = "linux")]
        {
            use std::os::unix::fs::PermissionsExt;
            file.as_file()
                .set_permissions(fs::Permissions::from_mode(0o755))?;
            // Serialize installers on the original inode and reject a replacement by another instance.
            let original = fs::File::open(&self.path)?;
            original
                .try_lock()
                .context("another instance is installing an update")?;
            ensure!(
                file_id(&original.metadata()?) == self.inode
                    && file_id(&fs::metadata(&self.path)?) == self.inode,
                "the app file changed while RustTalk was running"
            );
            file.as_file().sync_all()?;
            let installed = file.persist(&self.path)?;
            Ok(State::Installed(
                update.version,
                Self {
                    inode: file_id(&installed.metadata()?),
                    path: self.path,
                },
            ))
        }
        #[cfg(target_os = "windows")]
        {
            file.as_file().sync_all()?;
            drop(file.persist(&self.path)?);
            Command::new(&self.path)
                .args(["/RUSTTALKUPDATE=1", "/SILENT", "/NORESTART", "/LOG"])
                .spawn()
                .context("start verified RustTalk installer")?;
            Ok(State::InstallerStarted(update.version))
        }
        #[cfg(target_os = "macos")]
        {
            file.as_file().sync_all()?;
            install_dmg(file.path(), &self, &update.version, &cache)?;
            Ok(State::Installed(update.version, self))
        }
        // `Target::new` fails on phones, so nothing gets here.
        #[cfg(any(target_os = "android", target_os = "ios"))]
        unreachable!("phones have no update target")
    }

    #[cfg(target_os = "macos")]
    fn restart(&self) -> Result<()> {
        // `-n`: this instance is still exiting, and would otherwise just be activated.
        Command::new("/usr/bin/open")
            .arg("-n")
            .arg(&self.path)
            .spawn()
            .context("restart RustTalk")?;
        Ok(())
    }

    #[cfg(target_os = "linux")]
    fn restart(&self) -> Result<()> {
        ensure!(
            file_id(&fs::metadata(&self.path)?) == self.inode,
            "the updated app file has changed"
        );
        Command::new(&self.path)
            .env_remove("APPDIR")
            .env_remove("APPIMAGE")
            .spawn()
            .context("restart RustTalk")?;
        Ok(())
    }
}

pub fn restart_if_requested() -> Result<()> {
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    if let Some(target) = RESTART.get() {
        target.restart()?;
    }
    Ok(())
}

/// The `.app` bundle that holds `executable` (`<bundle>.app/Contents/MacOS/<name>`).
#[cfg(target_os = "macos")]
fn bundle_of(executable: &Path) -> Option<PathBuf> {
    let macos = executable.parent().filter(|dir| dir.ends_with("MacOS"))?;
    let contents = macos.parent().filter(|dir| dir.ends_with("Contents"))?;
    let bundle = contents.parent()?;
    (bundle.extension()? == "app").then(|| bundle.to_path_buf())
}

/// The bundle identifier in `deploy/release/Info.plist`.
#[cfg(target_os = "macos")]
const BUNDLE_ID: &str = "io.github.rusttalkcrew.rusttalk";

/// `~/Library/Caches/<bundle id>`, readable by this user only.
#[cfg(target_os = "macos")]
fn cache_dir() -> Result<PathBuf> {
    use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
    let home = env::var_os("HOME").context("locate the home folder")?;
    let dir = Path::new(&home).join("Library/Caches").join(BUNDLE_ID);
    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(&dir)?;
    let metadata = fs::symlink_metadata(&dir)?;
    ensure!(metadata.is_dir(), "the update cache is not a folder");
    if metadata.permissions().mode() & 0o077 != 0 {
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o700))?;
    }
    Ok(dir)
}

/// Replaces the installed bundle with the `RustTalk.app` in the verified disk image
/// `dmg`. The new bundle is copied next to the old one and checked first, so the
/// swap happens on one volume and any failure leaves the old app in place. `cache`
/// is the private folder that holds the per-bundle install lock.
#[cfg(target_os = "macos")]
fn install_dmg(dmg: &Path, target: &Target, version: &Version, cache: &Path) -> Result<()> {
    let bundle = &target.path;
    let (_lock, key) = lock_bundle(cache, bundle)?;
    let parent = bundle.parent().context("locate the app folder")?;
    clean_stale(parent, ".rusttalk-update");
    clean_stale(cache, "download");
    let stage = tempfile::Builder::new()
        .prefix(".rusttalk-update")
        .tempdir_in(parent)
        .context("RustTalk's folder must allow creating files")?;
    let mount = stage.path().join("mount");
    let new = stage.path().join("RustTalk.app");
    fs::create_dir(&mount)?;
    run(Command::new("/usr/bin/hdiutil")
        .args(["attach", "-nobrowse", "-readonly", "-noautoopen", "-quiet"])
        .arg("-mountpoint")
        .arg(&mount)
        .arg(dmg))
    .context("open the update disk image")?;
    // `ditto` keeps the bundle's code signature and symlinks intact.
    let copied = run(Command::new("/usr/bin/ditto")
        .arg(mount.join("RustTalk.app"))
        .arg(&new));
    let detached = run(Command::new("/usr/bin/hdiutil")
        .args(["detach", "-quiet"])
        .arg(&mount))
    .or_else(|_| {
        run(Command::new("/usr/bin/hdiutil")
            .args(["detach", "-force", "-quiet"])
            .arg(&mount))
    });
    copied.context("copy the new RustTalk.app")?;
    detached.context("close the update disk image")?;
    check_bundle(&new, version)?;
    // A window that started before another one updated must not put its older
    // download over the newer app.
    ensure!(
        fs::symlink_metadata(bundle).is_ok_and(|now| file_id(&now) == target.inode),
        "RustTalk was updated in another window; restart it first"
    );
    // After the exchange the old bundle sits in the stage, which is removed on return.
    match swap(&new, bundle) {
        Ok(()) => Ok(()),
        // The volume can't exchange in one step.
        Err(error) if matches!(error.raw_os_error(), Some(libc::ENOTSUP | libc::EINVAL)) => {
            let old = parent.join(format!(".RustTalk-old-{key}.app"));
            replace_bundle(&new, bundle, &old, |from, to| fs::rename(from, to))
        }
        Err(error) => Err(anyhow::Error::new(error).context("put the new RustTalk.app in place")),
    }
}

/// Removes what an install that was cut short (a crash, a forced quit) left in
/// `dir`: entries named `prefix*` older than an hour, so a running download or
/// install is never touched. A disk image still mounted in one is closed first.
#[cfg(target_os = "macos")]
fn clean_stale(dir: &Path, prefix: &str) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let stale = entry.file_name().to_string_lossy().starts_with(prefix)
            && entry
                .metadata()
                .and_then(|metadata| metadata.modified())
                .is_ok_and(|time| time.elapsed().is_ok_and(|age| age > STALE));
        if stale {
            let mount = entry.path().join("mount");
            if mount.is_dir() {
                let _ = run(Command::new("/usr/bin/hdiutil")
                    .args(["detach", "-force", "-quiet"])
                    .arg(&mount));
            }
            let _ = fs::remove_dir_all(entry.path());
        }
    }
}

/// Longer than any download or install takes (the HTTP timeout is five minutes).
#[cfg(target_os = "macos")]
const STALE: Duration = Duration::from_secs(60 * 60);

/// Takes the install lock for `bundle`, so two windows can't install into it at
/// once. Returns the held lock and the bundle's short key.
#[cfg(target_os = "macos")]
fn lock_bundle(cache: &Path, bundle: &Path) -> Result<(fs::File, String)> {
    let mut key = hex::encode(Sha256::digest(bundle.as_os_str().as_encoded_bytes()));
    key.truncate(16);
    let lock = fs::File::create(cache.join(format!("install-{key}.lock")))?;
    lock.try_lock()
        .context("another RustTalk window is installing an update")?;
    Ok((lock, key))
}

/// Checks that `bundle` is a validly signed RustTalk.app of exactly `version`, whose
/// executable is a regular file and whose symlinks stay inside it.
#[cfg(target_os = "macos")]
fn check_bundle(bundle: &Path, version: &Version) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let not_app = "the update is not a RustTalk app";
    for dir in [
        bundle.to_owned(),
        bundle.join("Contents"),
        bundle.join("Contents/MacOS"),
    ] {
        ensure!(fs::symlink_metadata(dir).is_ok_and(|m| m.is_dir()), not_app);
    }
    let plist = bundle.join("Contents/Info.plist");
    ensure!(
        fs::symlink_metadata(&plist).is_ok_and(|m| m.is_file()),
        "the update has no app information"
    );
    let read = |key: &str| -> Result<String> {
        let output = Command::new("/usr/bin/plutil")
            .args(["-extract", key, "raw", "-o", "-"])
            .arg(&plist)
            .output()?;
        ensure!(output.status.success(), "the update has no {key}");
        Ok(String::from_utf8(output.stdout)?.trim().to_owned())
    };
    ensure!(read("CFBundleIdentifier")? == BUNDLE_ID, not_app);
    ensure!(
        read("CFBundleShortVersionString")? == version.to_string(),
        "the update is not version {version}"
    );
    let executable =
        fs::symlink_metadata(bundle.join("Contents/MacOS/rusttalk-client")).context(not_app)?;
    ensure!(
        executable.is_file() && executable.permissions().mode() & 0o111 != 0,
        not_app
    );
    let root = fs::canonicalize(bundle)?;
    let mut dirs = vec![root.clone()];
    while let Some(dir) = dirs.pop() {
        for entry in fs::read_dir(dir)? {
            let entry = entry?;
            let kind = entry.file_type()?;
            if kind.is_symlink() {
                ensure!(
                    fs::canonicalize(entry.path()).is_ok_and(|to| to.starts_with(&root)),
                    "the update links outside its app"
                );
            } else if kind.is_dir() {
                dirs.push(entry.path());
            }
        }
    }
    run(Command::new("/usr/bin/codesign")
        .args(["--verify", "--deep", "--strict"])
        .arg(bundle))
    .context("the update's signature is broken")
}

/// Exchanges `new` and `bundle` in one step, so the app path is never missing.
#[cfg(target_os = "macos")]
fn swap(new: &Path, bundle: &Path) -> std::io::Result<()> {
    use std::os::unix::ffi::OsStrExt;
    let c_path = |path: &Path| std::ffi::CString::new(path.as_os_str().as_bytes());
    let (from, to) = (c_path(new)?, c_path(bundle)?);
    // SAFETY: both are NUL-terminated C strings that outlive the call.
    if unsafe { libc::renamex_np(from.as_ptr(), to.as_ptr(), libc::RENAME_SWAP) } == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

/// For volumes without an atomic exchange: moves the old bundle to `old`, then `new`
/// into place. The old bundle is deleted only once the new one is in place; if it
/// can't even be put back, it stays at `old` and the error says where.
#[cfg(target_os = "macos")]
fn replace_bundle(
    new: &Path,
    bundle: &Path,
    old: &Path,
    rename: impl Fn(&Path, &Path) -> std::io::Result<()>,
) -> Result<()> {
    rename(bundle, old).map_err(|error| {
        if matches!(error.raw_os_error(), Some(libc::ENOTEMPTY | libc::EEXIST)) {
            anyhow!(
                "remove the leftover copy at {} and try again",
                old.display()
            )
        } else {
            anyhow::Error::new(error).context("move the old RustTalk.app aside")
        }
    })?;
    if let Err(error) = rename(new, bundle) {
        return match rename(old, bundle) {
            Ok(()) => Err(anyhow::Error::new(error).context("put the new RustTalk.app in place")),
            Err(_) => Err(anyhow!(
                "couldn't put RustTalk back; your copy is at {}",
                old.display()
            )),
        };
    }
    let _ = fs::remove_dir_all(old);
    Ok(())
}

#[cfg(target_os = "macos")]
fn run(command: &mut Command) -> Result<()> {
    let status = command.status()?;
    ensure!(
        status.success(),
        "{:?} failed: {status}",
        command.get_program()
    );
    Ok(())
}

/// Finds the AppImage file that this process was started from, or `None` when the
/// executable is not running from an AppImage mount.
///
/// `APPIMAGE` and `APPDIR` are inherited by every child of another AppImage (an
/// editor, a launcher), so neither is trusted on its own: the path is only accepted
/// when the AppImage runtime's FUSE mount that holds this executable is named after
/// it, serves the squashfs image embedded in that exact file, and a running process
/// (the runtime's FUSE server) was started from that same file.
#[cfg(target_os = "linux")]
fn running_appimage(executable: &Path) -> Result<Option<Target>> {
    let Some(appdir) = env::var_os("APPDIR")
        .and_then(|dir| fs::canonicalize(dir).ok())
        .filter(|dir| {
            fs::canonicalize(dir.join("usr/bin/rusttalk-client"))
                .is_ok_and(|client| client == executable)
        })
    else {
        return Ok(None);
    };
    let refused = "RustTalk can't confirm which AppImage it was started from";
    let image = env::var_os("APPIMAGE")
        .and_then(|image| fs::canonicalize(image).ok())
        .context(refused)?;
    let mountinfo = fs::read_to_string("/proc/self/mountinfo").context(refused)?;
    let mut mount = mount_at(&mountinfo, &appdir).context(refused)?;
    let stat = statvfs(&appdir).context(refused)?;
    (mount.block_size, mount.blocks, mount.files) = stat;
    prove_image(&image, &mount).context(refused)?;
    let inode = file_id(&fs::metadata(&image)?);
    ensure!(process_runs(inode), refused);
    Ok(Some(Target { path: image, inode }))
}

#[cfg(target_os = "linux")]
fn statvfs(path: &Path) -> Result<(u64, u64, u64)> {
    use std::os::unix::ffi::OsStrExt;
    let path = std::ffi::CString::new(path.as_os_str().as_bytes())?;
    let mut stat = std::mem::MaybeUninit::<libc::statvfs>::uninit();
    // SAFETY: `path` is NUL-terminated and `stat` is a valid output buffer.
    ensure!(
        unsafe { libc::statvfs(path.as_ptr(), stat.as_mut_ptr()) } == 0,
        "statvfs failed"
    );
    // SAFETY: statvfs succeeded and initialised the buffer.
    let stat = unsafe { stat.assume_init() };
    Ok((stat.f_bsize, stat.f_blocks, stat.f_files))
}

/// The AppImage FUSE mount that serves an AppDir, as the kernel and the runtime report it.
#[cfg(target_os = "linux")]
#[derive(Debug, Default)]
struct Mount {
    fstype: String,
    source: String,
    block_size: u64,
    blocks: u64,
    files: u64,
}

#[cfg(target_os = "linux")]
fn mount_at(mountinfo: &str, dir: &Path) -> Option<Mount> {
    use std::os::unix::ffi::OsStrExt;
    // Octal escapes (`\040` for a space) are used for whitespace and backslashes.
    fn unescape(field: &str) -> Vec<u8> {
        let bytes = field.as_bytes();
        let mut out = Vec::with_capacity(bytes.len());
        let mut i = 0;
        while i < bytes.len() {
            match bytes.get(i..i + 4) {
                Some([b'\\', digits @ ..]) if digits.iter().all(|d| (b'0'..=b'7').contains(d)) => {
                    out.push(
                        digits
                            .iter()
                            .fold(0u8, |n, d| n.wrapping_mul(8) + (d - b'0')),
                    );
                    i += 4;
                }
                _ => {
                    out.push(bytes[i]);
                    i += 1;
                }
            }
        }
        out
    }
    // The last matching line is the mount that is visible at that path.
    mountinfo.lines().rev().find_map(|line| {
        let (mounted, rest) = line.split_once(" - ")?;
        if unescape(mounted.split(' ').nth(4)?) != dir.as_os_str().as_bytes() {
            return None;
        }
        let mut rest = rest.split(' ');
        Some(Mount {
            fstype: String::from_utf8(unescape(rest.next()?)).ok()?,
            source: String::from_utf8(unescape(rest.next()?)).ok()?,
            ..Mount::default()
        })
    })
}

/// Checks that `image` is a type 2 AppImage whose embedded squashfs is the file
/// system mounted at `mount`.
#[cfg(target_os = "linux")]
fn prove_image(image: &Path, mount: &Mount) -> Result<()> {
    use std::io::{Read, Seek, SeekFrom};
    let name = image
        .file_name()
        .and_then(|name| name.to_str())
        .context("unnamed AppImage")?;
    ensure!(
        mount.fstype == format!("fuse.{name}") && mount.source == name,
        "the app folder is not mounted from {name}"
    );
    let mut file = fs::File::open(image)?;
    let mut header = [0u8; 64];
    file.read_exact(&mut header)?;
    ensure!(
        header[..4] == *b"\x7fELF"
            && header[4] == 2
            && header[5] == 1
            && header[8..11] == *b"AI\x02",
        "not a 64-bit type 2 AppImage"
    );
    let le64 = |at: usize| u64::from_le_bytes(header[at..at + 8].try_into().expect("8 bytes"));
    let le16 = |at: usize| u64::from(u16::from_le_bytes([header[at], header[at + 1]]));
    // The squashfs image starts right after the runtime's section headers.
    let offset = le16(0x3a)
        .checked_mul(le16(0x3c))
        .and_then(|size| size.checked_add(le64(0x28)))
        .context("invalid AppImage runtime")?;
    let mut superblock = [0u8; 48];
    file.seek(SeekFrom::Start(offset))?;
    file.read_exact(&mut superblock)?;
    let le32 = |at: usize| {
        u64::from(u32::from_le_bytes(
            superblock[at..at + 4].try_into().expect("4 bytes"),
        ))
    };
    ensure!(
        &superblock[..4] == b"hsqs",
        "no squashfs image in the AppImage"
    );
    let (inodes, block_size) = (le32(4), le32(12));
    let used = u64::from_le_bytes(superblock[40..48].try_into().expect("8 bytes"));
    ensure!(
        block_size > 0
            && mount.block_size == block_size
            && mount.files == inodes
            && (used / block_size..=used / block_size + 1).contains(&mount.blocks),
        "the app folder is not served from {name}"
    );
    Ok(())
}

/// Whether any visible process was started from the file with this device and inode.
#[cfg(target_os = "linux")]
fn process_runs(inode: (u64, u64)) -> bool {
    fs::read_dir("/proc").is_ok_and(|entries| {
        entries.flatten().any(|entry| {
            entry
                .file_name()
                .to_str()
                .is_some_and(|pid| pid.bytes().all(|b| b.is_ascii_digit()))
                && fs::metadata(entry.path().join("exe")).is_ok_and(|exe| file_id(&exe) == inode)
        })
    })
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn file_id(metadata: &fs::Metadata) -> (u64, u64) {
    use std::os::unix::fs::MetadataExt;
    (metadata.dev(), metadata.ino())
}

fn runtime() -> Result<tokio::runtime::Runtime> {
    Ok(tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?)
}

#[cfg(target_os = "linux")]
pub fn check_now(config: &Path) -> Result<()> {
    let target = Target::new(config)?;
    let url = if LOCAL_TEST {
        env::var("RUSTTALK_UPDATE_URL").unwrap_or(Settings::load(&config.join("updates.json"))?.url)
    } else {
        Settings::load(&config.join("updates.json"))?.url
    };
    runtime()?.block_on(async {
        if let Found::Newer(update) = check(&url, None).await? {
            let version = update.version.clone();
            target.install(update, Arc::default()).await?;
            println!("Installed RustTalk {version}");
        } else {
            println!("RustTalk is up to date");
        }
        Ok(())
    })
}

/// How much of the download is on disk.
struct Progress {
    done: Arc<AtomicU64>,
    total: u64,
}

impl Progress {
    fn fraction(&self) -> f32 {
        (self.done.load(Ordering::Relaxed) as f64 / self.total as f64) as f32
    }
}

enum State {
    Idle,
    /// A check (no progress) or an install (with progress) runs in the background.
    Working(Receiver<Result<State>>, Option<Progress>),
    Available(Update),
    /// Up to date, with the ETag of the manifest that said so.
    Current(Option<String>),
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    Installed(Version, Target),
    #[cfg(target_os = "windows")]
    InstallerStarted(Version),
    Failed(String),
}

pub struct Updater {
    config: PathBuf,
    settings: Settings,
    target: Result<Target>,
    state: State,
    next_check: Option<Instant>,
    /// The confirmation dialog is open, and stays open to show progress or a failure.
    asking: bool,
    /// The banner was closed for this version; a newer one shows it again.
    dismissed: Option<Version>,
    /// "Update after the call" was picked: install once the call ends.
    after_call: bool,
}

impl Updater {
    pub fn new(config: &Path) -> Self {
        let path = config.join("updates.json");
        let (settings, state) = match Settings::load(&path) {
            Ok(settings) => (settings, State::Idle),
            Err(error) => (
                Settings::default(),
                failure(error.context("load update settings")),
            ),
        };
        Self {
            config: path,
            settings,
            target: Target::new(config),
            state,
            next_check: None,
            asking: false,
            dismissed: None,
            after_call: false,
        }
    }

    fn start_check(&mut self, ctx: &egui::Context) {
        let url = self.settings.url.clone();
        // Only "nothing new" is worth asking about again; any other state needs the full answer.
        let seen = match &self.state {
            State::Current(etag) => etag.clone(),
            _ => None,
        };
        self.state = background(ctx, None, move || {
            Ok(match runtime()?.block_on(check(&url, seen.as_deref()))? {
                Found::Unchanged => State::Current(seen),
                Found::Current(etag) => State::Current(etag),
                Found::Newer(update) => State::Available(update),
            })
        });
    }

    /// Runs the install of the available update in the background.
    fn start_install(
        &mut self,
        ctx: &egui::Context,
        work: impl FnOnce(Update, Arc<AtomicU64>) -> Result<State> + Send + 'static,
    ) {
        let State::Available(update) = std::mem::replace(&mut self.state, State::Idle) else {
            return;
        };
        let done = Arc::<AtomicU64>::default();
        let progress = Progress {
            done: done.clone(),
            total: update.artifact.size,
        };
        self.state = background(ctx, Some(progress), move || work(update, done));
    }

    fn confirm(&mut self, ctx: &egui::Context) {
        if let Ok(target) = &self.target {
            let target = target.clone();
            self.start_install(ctx, move |update, done| {
                runtime()?.block_on(target.install(update, done))
            });
        }
    }

    /// Closes the dialog; after a failure it looks again, so a retry is one click away.
    fn dismiss(&mut self, ctx: &egui::Context) {
        self.asking = false;
        if matches!(self.state, State::Failed(_)) && !self.settings.url.trim().is_empty() {
            self.start_check(ctx);
        }
    }

    /// Whether a background job or the restart owns the app right now.
    fn busy(&self) -> bool {
        match self.state {
            State::Working(..) => true,
            #[cfg(any(target_os = "linux", target_os = "macos"))]
            State::Installed(..) => true,
            #[cfg(target_os = "windows")]
            State::InstallerStarted(_) => true,
            _ => false,
        }
    }

    pub fn poll(&mut self, ctx: &egui::Context) {
        if let State::Working(receiver, _) = &self.state {
            match receiver.try_recv() {
                Ok(result) => self.state = result.unwrap_or_else(failure),
                Err(TryRecvError::Disconnected) => {
                    self.state = failure(anyhow!("update worker stopped"))
                }
                Err(TryRecvError::Empty) => ctx.request_repaint_after(Duration::from_millis(100)),
            }
        }
        // The new app starts once this one has exited (`restart_if_requested`).
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        if let State::Installed(_, target) = &self.state {
            let _ = RESTART.set(target.clone());
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
        }
        // The installer restarts RustTalk itself once it is closed.
        #[cfg(target_os = "windows")]
        if matches!(self.state, State::InstallerStarted(_)) {
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
        }
        let ready = rusttalk_core::route::is_active()
            && self.settings.automatic
            && !self.settings.url.trim().is_empty()
            && self.target.is_ok()
            && !self.busy()
            && !self.asking
            && !self.available();
        if ready {
            let now = Instant::now();
            if check_due(self.next_check, now) {
                self.next_check = Some(now + self.settings.check_every());
                self.start_check(ctx);
            } else if let Some(next) = self.next_check {
                ctx.request_repaint_after(next.saturating_duration_since(now));
            }
        }
    }

    /// Clear cached update clients/status when the network route changes so the next
    /// check and download use the new route.
    pub fn on_route_changed(&mut self) {
        if !self.busy() {
            self.state = State::Idle;
        }
        self.next_check = None;
    }

    pub fn available(&self) -> bool {
        matches!(self.state, State::Available(_))
    }

    /// Whether the banner shows: a verified newer version the person has not closed it for.
    fn banner_wanted(&self) -> bool {
        matches!(&self.state, State::Available(update) if self.dismissed.as_ref() != Some(&update.version))
    }

    /// Whether a deferred "update after the call" should start now. Forgets the
    /// request once there is nothing left to update.
    fn due_after_call(&mut self, in_call: bool) -> bool {
        self.after_call &= self.available();
        let due = self.after_call && !in_call;
        self.after_call &= !due;
        due
    }

    /// Starts the install with the progress dialog; the app restarts when it is done.
    fn begin(&mut self, ctx: &egui::Context) {
        self.asking = true;
        self.confirm(ctx);
    }

    /// The banner button: install now, or once the call is over. A restart must never
    /// cut off a call.
    fn choose(&mut self, ctx: &egui::Context, in_call: bool) {
        if in_call {
            self.after_call = true;
        } else {
            self.begin(ctx);
        }
    }

    /// A slim bar across the top of the window while an update waits. It pushes the
    /// layout down instead of floating, so it never covers the call controls.
    pub fn banner(&mut self, ui: &mut egui::Ui, in_call: bool) {
        if self.due_after_call(in_call) {
            self.begin(ui.ctx());
        }
        let State::Available(update) = &self.state else {
            return;
        };
        if !self.banner_wanted() {
            return;
        }
        let version = update.version.clone();
        let (c, accent) = (crate::ui::colors(ui), crate::ui::accent(ui));
        let (mut go, mut close) = (false, false);
        let bar = egui::Panel::top("update banner")
            .frame(
                egui::Frame::new()
                    .fill(c.main.lerp_to_gamma(accent.base, 0.18))
                    .inner_margin(egui::Margin::symmetric(
                        crate::ui::SPACE_M as i8,
                        crate::ui::SPACE_XS as i8,
                    )),
            )
            .show(ui, |ui| {
                // A fixed row height, so the text centres against the button.
                let row = egui::vec2(ui.available_width(), crate::ui::CONTROL_HEIGHT);
                ui.allocate_ui_with_layout(
                    row,
                    egui::Layout::left_to_right(egui::Align::Center),
                    |ui| {
                        let text =
                            crate::ui::bold(t!("update.banner", version = version)).color(c.strong);
                        ui.add(egui::Label::new(text).truncate());
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            close = crate::ui::small_close(ui, t!("common.dismiss")).clicked();
                            if self.after_call {
                                ui.label(t!("update.banner_scheduled"));
                            } else {
                                let label = if in_call {
                                    t!("update.banner_after_call")
                                } else {
                                    t!("update.banner_action")
                                };
                                go = crate::ui::primary(ui, label).clicked();
                            }
                        });
                    },
                );
            });
        let rect = bar.response.rect;
        let line = egui::Stroke::new(1.0, accent.base.gamma_multiply(0.6));
        ui.painter().hline(rect.x_range(), rect.bottom(), line);
        if close {
            self.dismissed = Some(version);
            self.after_call = false;
        } else if go {
            self.choose(ui.ctx(), in_call);
        }
    }

    /// The Update button, shown while a verified newer version waits.
    pub fn button(&mut self, ui: &mut egui::Ui) {
        if let State::Available(update) = &self.state {
            let label = t!("update.update_to", version = update.version);
            ui.with_layout(
                egui::Layout::top_down_justified(egui::Align::Center),
                |ui| {
                    if crate::ui::primary(ui, &label).clicked() {
                        self.asking = true;
                    }
                },
            );
        }
    }

    /// The confirmation, then the download's progress, then any failure.
    pub fn dialog(&mut self, ctx: &egui::Context) {
        if !self.asking {
            return;
        }
        let title = match &self.state {
            State::Available(update) => t!("update.title", version = update.version),
            State::Failed(_) => t!("update.failed").to_owned(),
            State::Working(..) => t!("update.updating").to_owned(),
            _ => t!("update.restarting").to_owned(),
        };
        let (mut go, mut later) = (false, false);
        let (_, closed) = crate::ui::dialog(ctx, "update", &title, |ui| match &self.state {
            State::Available(update) => {
                ui.label(t!("update.restarts"));
                if let Some(url) = release_notes(&update.base, &update.version) {
                    ui.hyperlink_to(t!("update.whats_new"), url);
                }
                crate::ui::footer(ui, |ui| {
                    go = crate::ui::primary(ui, t!("update.update")).clicked();
                    later = crate::ui::secondary(ui, t!("update.later")).clicked();
                });
            }
            State::Failed(error) => {
                crate::ui::error_text(ui, Some(error));
                crate::ui::footer(ui, |ui| {
                    later = crate::ui::secondary(ui, t!("common.close")).clicked()
                });
            }
            State::Working(_, Some(progress)) => {
                ui.add(egui::ProgressBar::new(progress.fraction()).show_percentage());
            }
            _ => {
                ui.spinner();
            }
        });
        let working = matches!(self.state, State::Working(..));
        if go {
            self.confirm(ctx);
        } else if later || (closed && !working) {
            self.dismiss(ctx);
        }
    }

    pub fn settings(&mut self, ui: &mut egui::Ui) {
        let can_check = !self.busy();
        crate::ui::page_section(ui, t!("settings.updates"));
        ui.add_enabled_ui(can_check, |ui| {
            let mut changed = crate::ui::toggle_row(
                ui,
                &mut self.settings.automatic,
                t!("update.automatic"),
                t!("update.automatic_help"),
            )
            .changed();
            let every = crate::ui::setting_row(
                ui,
                t!("update.interval"),
                t!("update.interval_help"),
                |ui| {
                    ui.add_enabled_ui(self.settings.automatic, |ui| {
                        ui.scope(|ui| {
                            ui.set_width(300.0);
                            crate::ui::segmented(
                                ui,
                                &mut self.settings.check_minutes,
                                &[
                                    (CHECK_MINUTES[0], t!("update.every_15m"), ""),
                                    (CHECK_MINUTES[1], t!("update.every_1h"), ""),
                                    (CHECK_MINUTES[2], t!("update.every_6h"), ""),
                                    (CHECK_MINUTES[3], t!("update.every_24h"), ""),
                                ],
                            )
                        })
                        .inner
                    })
                    .inner
                },
            );
            if every {
                // A shorter interval takes effect now; a longer one after the next check.
                let soonest = Instant::now() + self.settings.check_every();
                self.next_check = self.next_check.map(|next| next.min(soonest));
                changed = true;
            }
            let url =
                crate::ui::setting_row(ui, t!("update.address"), t!("update.address_help"), |ui| {
                    ui.add(
                        crate::ui::field(&mut self.settings.url)
                            .hint_text(t!("update.address_hint"))
                            .desired_width(280.0),
                    )
                });
            url.widget_info(|| {
                egui::WidgetInfo::labeled(
                    egui::accesskit::Role::TextInput,
                    true,
                    t!("update.address"),
                )
            });
            if url.changed() && matches!(self.state, State::Available(_) | State::Current(_)) {
                self.state = State::Idle;
            }
            let check = ui
                .add_enabled_ui(
                    self.target.is_ok() && !self.settings.url.trim().is_empty(),
                    |ui| crate::ui::secondary(ui, t!("update.check_now")),
                )
                .inner
                .clicked();
            if changed || url.lost_focus() || check {
                if let Err(error) = self.settings.save(&self.config) {
                    self.state = failure(error.context("save update settings"));
                } else if check {
                    self.start_check(ui.ctx());
                }
            }
        });
        if let Err(error) = &self.target {
            ui.weak(error.to_string());
        }
        match &self.state {
            State::Idle => {}
            State::Working(_, None) => {
                ui.spinner();
            }
            State::Working(_, Some(progress)) => {
                ui.add(egui::ProgressBar::new(progress.fraction()).show_percentage());
            }
            State::Available(update) => {
                if crate::ui::primary(ui, &t!("update.update_to", version = update.version))
                    .clicked()
                {
                    self.asking = true;
                }
            }
            State::Current(_) => {
                ui.label(t!("update.up_to_date"));
            }
            #[cfg(any(target_os = "linux", target_os = "macos"))]
            State::Installed(version, _) => {
                ui.label(t!("update.installed", version = version));
            }
            #[cfg(target_os = "windows")]
            State::InstallerStarted(version) => {
                ui.label(t!("update.installer", version = version));
            }
            State::Failed(error) => {
                ui.label(error);
            }
        }
    }
}

fn background(
    ctx: &egui::Context,
    progress: Option<Progress>,
    work: impl FnOnce() -> Result<State> + Send + 'static,
) -> State {
    let (send, receive) = mpsc::channel();
    let ctx = ctx.clone();
    match std::thread::Builder::new()
        .name("rusttalk-update".into())
        .spawn(move || {
            let _ = send.send(work());
            ctx.request_repaint();
        }) {
        Ok(_) => State::Working(receive, progress),
        Err(error) => failure(anyhow::Error::new(error).context("start update worker")),
    }
}

fn failure(error: anyhow::Error) -> State {
    let message = format!("{error:#}");
    tracing::warn!(reason = %message, "update failed");
    State::Failed(message)
}

#[cfg(test)]
mod tests {
    use super::*;
    use ed25519_dalek::{Signer, SigningKey};

    #[test]
    fn manifest_signature_covers_exact_bytes_and_artifact_hash() {
        let key = SigningKey::from_bytes(&[1; 32]);
        let mut manifest = br#"{"version":"0.1.1","platforms":{"linux-x86_64":{"file":"app","size":3,"sha256":"ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"}}}"#.to_vec();
        let sig = key.sign(&manifest).to_bytes();
        let parsed = verify(&key.verifying_key(), &manifest, &sig).unwrap();
        let artifact = &parsed.platforms["linux-x86_64"];
        assert!(artifact.verify(3, &Sha256::digest(b"abc")).is_ok());
        assert!(artifact.verify(3, &Sha256::digest(b"abd")).is_err());
        assert!(artifact.verify(2, &Sha256::digest(b"abc")).is_err());
        let other = SigningKey::from_bytes(&[2; 32]);
        assert!(verify(&other.verifying_key(), &manifest, &sig).is_err());
        manifest.push(b'\n');
        assert!(verify(&key.verifying_key(), &manifest, &sig).is_err());
        manifest.pop();
        let hash_start = manifest
            .windows(2)
            .position(|bytes| bytes == b"ba")
            .unwrap();
        manifest[hash_start] = b'c';
        assert!(verify(&key.verifying_key(), &manifest, &sig).is_err());
    }

    fn fake_update(base: &str) -> Update {
        Update {
            base: Url::parse(base).unwrap(),
            version: Version::new(0, 2, 0),
            artifact: Artifact {
                file: "app".into(),
                size: 4,
                sha256: String::new(),
            },
        }
    }

    fn updater(state: State) -> Updater {
        Updater {
            config: PathBuf::new(),
            settings: Settings {
                automatic: true,
                url: "https://example.com/".into(),
                check_minutes: default_check_minutes(),
            },
            target: Err(anyhow!("no target in tests")),
            state,
            next_check: None,
            asking: false,
            dismissed: None,
            after_call: false,
        }
    }

    /// Polls until the background job has answered.
    fn settle(updater: &mut Updater, ctx: &egui::Context) {
        for _ in 0..200 {
            updater.poll(ctx);
            if !matches!(updater.state, State::Working(..)) {
                return;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        panic!("update job did not finish");
    }

    #[test]
    fn schedule_checks_at_startup_then_every_chosen_interval() {
        let start = Instant::now();
        let every = Settings::default().check_every();
        assert!(check_due(None, start));
        let next = Some(start + every);
        assert!(!check_due(next, start + every - Duration::from_secs(1)));
        assert!(check_due(next, start + every));
        assert_eq!(every, Duration::from_secs(15 * 60));
    }

    #[test]
    fn settings_saved_before_the_interval_existed_check_every_15_minutes() {
        let old: Settings = serde_json::from_str(r#"{"automatic":true,"url":"u"}"#).unwrap();
        assert_eq!(old.check_minutes, 15);
        assert_eq!(old.check_every(), Duration::from_secs(900));
        let hourly: Settings = serde_json::from_str(r#"{"url":"u","check_minutes":60}"#).unwrap();
        assert_eq!(hourly.check_every(), Duration::from_secs(3600));
        // Every offered choice round-trips, and a hand-edited value is never shorter than 15 minutes.
        for minutes in CHECK_MINUTES {
            let saved = serde_json::to_string(&Settings {
                check_minutes: minutes,
                ..Settings::default()
            })
            .unwrap();
            let back: Settings = serde_json::from_str(&saved).unwrap();
            assert_eq!(back.check_every(), Duration::from_secs(minutes * 60));
        }
        let zero: Settings = serde_json::from_str(r#"{"url":"u","check_minutes":0}"#).unwrap();
        assert_eq!(zero.check_every(), Duration::from_secs(900));
    }

    #[test]
    fn banner_shows_for_an_undismissed_available_version() {
        let mut updater = updater(State::Idle);
        assert!(!updater.banner_wanted());
        updater.state = State::Available(fake_update("https://example.com/"));
        assert!(updater.banner_wanted());
        // Closed for 0.2.0: stays hidden, however long the app runs.
        updater.dismissed = Some(Version::new(0, 2, 0));
        assert!(!updater.banner_wanted());
        // A newer release shows it again.
        let State::Available(update) = &mut updater.state else {
            unreachable!()
        };
        update.version = Version::new(0, 3, 0);
        assert!(updater.banner_wanted());
        // Not available any more (installing, up to date): no banner.
        updater.dismissed = None;
        updater.state = State::Current(None);
        assert!(!updater.banner_wanted());
    }

    #[test]
    fn update_after_the_call_waits_for_the_call_to_end() {
        let ctx = egui::Context::default();
        let mut updater = updater(State::Available(fake_update("https://example.com/")));
        // In a call the button only schedules: nothing installs, no dialog opens.
        updater.choose(&ctx, true);
        assert!(updater.after_call && updater.available() && !updater.asking);
        assert!(!updater.due_after_call(true));
        assert!(updater.after_call);
        // The call ended: it is due once, then forgotten.
        assert!(updater.due_after_call(false));
        assert!(!updater.after_call && !updater.due_after_call(false));
        // Closing the banner or losing the update cancels a pending request.
        updater.after_call = true;
        updater.state = State::Idle;
        assert!(!updater.due_after_call(false) && !updater.after_call);
    }

    #[test]
    fn poll_starts_one_scheduled_check_and_respects_the_switch() {
        let _route = crate::route::test_lock();
        let ctx = egui::Context::default();
        // No usable target, so even a due check must not start.
        let mut idle = updater(State::Idle);
        idle.poll(&ctx);
        assert!(idle.next_check.is_none());
        // With a target the first poll starts the check and arms the next one.
        let dir = tempfile::tempdir().unwrap();
        let mut ready = updater(State::Idle);
        ready.target = Ok(Target {
            path: dir.path().join("app"),
            #[cfg(any(target_os = "linux", target_os = "macos"))]
            inode: (0, 0),
        });
        ready.settings.url = "http://invalid.test/".into();
        ready.poll(&ctx);
        assert!(ready.next_check.is_some());
        assert!(matches!(ready.state, State::Working(..)));
        settle(&mut ready, &ctx);
        assert!(matches!(ready.state, State::Failed(_)));
        // Not due again yet: the state stays put.
        ready.poll(&ctx);
        assert!(matches!(ready.state, State::Failed(_)));
        // Turned off: never due, even when the time has come.
        ready.settings.automatic = false;
        ready.next_check = None;
        ready.poll(&ctx);
        assert!(ready.next_check.is_none());
    }

    #[test]
    fn saved_choice_beats_the_default() {
        let saved: Settings = serde_json::from_str(r#"{"automatic":false,"url":"u"}"#).unwrap();
        assert!(!saved.automatic);
        let unset: Settings = serde_json::from_str(r#"{"url":"u"}"#).unwrap();
        assert_eq!(unset.automatic, default_automatic());
    }

    #[test]
    fn empty_saved_address_gets_the_defaults() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("updates.json");
        std::fs::write(&path, r#"{"automatic":false,"url":""}"#).unwrap();
        let loaded = Settings::load(&path).unwrap();
        assert_eq!(loaded.url, default_url());
        assert!(loaded.automatic);
    }

    #[test]
    fn update_flow_goes_from_available_to_failed() {
        let _route = crate::route::test_lock();
        let ctx = egui::Context::default();
        let mut updater = updater(State::Available(fake_update("https://example.com/")));
        assert!(updater.available());
        updater.asking = true;
        updater.start_install(&ctx, |_, done| {
            done.store(2, Ordering::Relaxed);
            Err(anyhow!("update size mismatch"))
        });
        assert!(matches!(updater.state, State::Working(_, Some(_))));
        assert!(!updater.available());
        settle(&mut updater, &ctx);
        assert!(matches!(&updater.state, State::Failed(e) if e == "update size mismatch"));
        // The dialog stays open to show the error; Close looks again.
        assert!(updater.asking);
        updater.dismiss(&ctx);
        assert!(!updater.asking);
        assert!(matches!(updater.state, State::Working(_, None)));
    }

    #[test]
    fn later_keeps_the_update_waiting() {
        let ctx = egui::Context::default();
        let mut updater = updater(State::Available(fake_update("https://example.com/")));
        updater.asking = true;
        updater.dismiss(&ctx);
        assert!(!updater.asking && updater.available());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn finished_install_goes_to_restart() {
        let _route = crate::route::test_lock();
        let ctx = egui::Context::default();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("app");
        fs::write(&path, b"app").unwrap();
        let target = Target {
            inode: file_id(&fs::metadata(&path).unwrap()),
            path,
        };
        let mut updater = updater(State::Available(fake_update("https://example.com/")));
        updater.asking = true;
        updater.start_install(&ctx, move |update, _| {
            Ok(State::Installed(update.version, target))
        });
        settle(&mut updater, &ctx);
        assert!(matches!(&updater.state, State::Installed(v, _) if *v == Version::new(0, 2, 0)));
        // Restarting is not interrupted by a scheduled check.
        assert!(updater.busy());
    }

    #[test]
    fn release_notes_come_from_the_github_release_directory() {
        let version = Version::new(0, 2, 0);
        let notes = |base: &str| release_notes(&Url::parse(base).unwrap(), &version);
        assert_eq!(
            notes("https://github.com/rusttalkcrew/rusttalk-next/releases/latest/download/")
                .unwrap()
                .as_str(),
            "https://github.com/rusttalkcrew/rusttalk-next/releases/tag/v0.2.0"
        );
        assert!(notes("https://example.com/updates/").is_none());
        assert!(notes("https://github.com/rusttalkcrew/").is_none());
        assert!(notes("https://github.com/a/b/issues/").is_none());
        assert!(notes("http://github.com/a/b/releases/latest/download/").is_none());
    }

    #[test]
    fn only_a_newer_release_can_install() {
        let current = Version::new(0, 1, 1);
        assert!(newer(&Version::new(0, 1, 2), &current).unwrap());
        assert!(!newer(&current, &current).unwrap());
        assert!(newer(&Version::new(0, 1, 0), &current).is_err());
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn bundle_is_found_from_its_executable() {
        assert_eq!(
            bundle_of(Path::new(
                "/Applications/RustTalk.app/Contents/MacOS/rusttalk-client"
            )),
            Some(PathBuf::from("/Applications/RustTalk.app"))
        );
        assert!(bundle_of(Path::new("/usr/local/bin/rusttalk-client")).is_none());
        assert!(bundle_of(Path::new("/tmp/RustTalk/Contents/MacOS/rusttalk-client")).is_none());
        assert!(bundle_of(Path::new("/tmp/RustTalk.app/MacOS/rusttalk-client")).is_none());
    }

    /// An unsigned bundle shaped like RustTalk.app, with a real program inside.
    #[cfg(target_os = "macos")]
    fn fake_bundle(path: &Path, id: &str, version: &str) {
        let macos = path.join("Contents/MacOS");
        fs::create_dir_all(&macos).unwrap();
        fs::copy("/usr/bin/true", macos.join("rusttalk-client")).unwrap();
        fs::write(
            path.join("Contents/Info.plist"),
            format!(
                r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
<key>CFBundleExecutable</key><string>rusttalk-client</string>
<key>CFBundleIdentifier</key><string>{id}</string>
<key>CFBundlePackageType</key><string>APPL</string>
<key>CFBundleShortVersionString</key><string>{version}</string>
<key>CFBundleVersion</key><string>{version}</string>
</dict></plist>
"#
            ),
        )
        .unwrap();
    }

    #[cfg(target_os = "macos")]
    fn signed_bundle(path: &Path, version: &str) {
        fake_bundle(path, BUNDLE_ID, version);
        run(Command::new("/usr/bin/codesign")
            .args(["--force", "--sign", "-"])
            .arg(path))
        .unwrap();
    }

    #[cfg(target_os = "macos")]
    fn version_of(bundle: &Path) -> String {
        let plist = fs::read_to_string(bundle.join("Contents/Info.plist")).unwrap();
        ["0.1.0", "0.2.0"]
            .into_iter()
            .find(|version| plist.contains(version))
            .unwrap()
            .to_owned()
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn only_a_signed_rusttalk_app_of_the_signed_version_passes() {
        let dir = tempfile::tempdir().unwrap();
        let version = Version::new(0, 2, 0);
        let good = dir.path().join("good.app");
        signed_bundle(&good, "0.2.0");
        check_bundle(&good, &version).unwrap();
        assert!(check_bundle(&good, &Version::new(0, 3, 0)).is_err());
        // Changed after signing: the signature seals Info.plist.
        let plist = good.join("Contents/Info.plist");
        let changed = fs::read_to_string(&plist)
            .unwrap()
            .replace("</dict>", "<key>LSUIElement</key><true/></dict>");
        fs::write(&plist, changed).unwrap();
        assert!(check_bundle(&good, &version).is_err());

        let other = dir.path().join("other.app");
        fake_bundle(&other, "com.example.other", "0.2.0");
        assert!(check_bundle(&other, &version).is_err());
        let missing = dir.path().join("missing.app");
        signed_bundle(&missing, "0.2.0");
        fs::remove_file(missing.join("Contents/Info.plist")).unwrap();
        assert!(check_bundle(&missing, &version).is_err());
        let escaping = dir.path().join("escaping.app");
        signed_bundle(&escaping, "0.2.0");
        std::os::unix::fs::symlink("/etc", escaping.join("Contents/link")).unwrap();
        assert!(check_bundle(&escaping, &version).is_err());
        let linked = dir.path().join("linked.app");
        signed_bundle(&linked, "0.2.0");
        let program = linked.join("Contents/MacOS/rusttalk-client");
        fs::remove_file(&program).unwrap();
        std::os::unix::fs::symlink("/usr/bin/true", &program).unwrap();
        assert!(check_bundle(&linked, &version).is_err());
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn swap_exchanges_the_two_bundles() {
        let dir = tempfile::tempdir().unwrap();
        let (bundle, new) = (dir.path().join("RustTalk.app"), dir.path().join("new.app"));
        fake_bundle(&bundle, BUNDLE_ID, "0.1.0");
        fake_bundle(&new, BUNDLE_ID, "0.2.0");
        swap(&new, &bundle).unwrap();
        assert_eq!(version_of(&bundle), "0.2.0");
        assert_eq!(version_of(&new), "0.1.0");
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn a_leftover_copy_is_named_and_kept() {
        let dir = tempfile::tempdir().unwrap();
        let bundle = dir.path().join("RustTalk.app");
        let (new, old) = (dir.path().join("new.app"), dir.path().join("old.app"));
        fake_bundle(&bundle, BUNDLE_ID, "0.1.0");
        fake_bundle(&new, BUNDLE_ID, "0.2.0");
        fake_bundle(&old, BUNDLE_ID, "0.0.9");
        let error =
            replace_bundle(&new, &bundle, &old, |from, to| fs::rename(from, to)).unwrap_err();
        assert!(error.to_string().contains("leftover"));
        assert!(error.to_string().contains(&old.display().to_string()));
        assert_eq!(version_of(&bundle), "0.1.0");
        assert!(old.exists() && new.exists());
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn only_stale_leftovers_are_cleaned() {
        let dir = tempfile::tempdir().unwrap();
        let make = |name: &str, age: Duration| {
            let path = dir.path().join(name);
            fs::create_dir(&path).unwrap();
            fs::write(path.join("file"), b"x").unwrap();
            let modified = std::time::SystemTime::now() - age;
            fs::File::open(&path)
                .unwrap()
                .set_modified(modified)
                .unwrap();
            path
        };
        let old_stage = make(".rusttalk-update1", STALE * 2);
        let running = make(".rusttalk-update2", Duration::ZERO);
        let other = make("Other.app", STALE * 2);
        clean_stale(dir.path(), ".rusttalk-update");
        assert!(!old_stage.exists());
        assert!(running.exists() && other.exists());
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn fallback_swap_never_deletes_the_only_copy() {
        let dir = tempfile::tempdir().unwrap();
        let bundle = dir.path().join("RustTalk.app");
        let (new, old) = (dir.path().join("new.app"), dir.path().join("old.app"));
        // Renames succeed only for the listed calls (1-based).
        let only = |allowed: &'static [usize]| {
            let calls = std::cell::Cell::new(0);
            move |from: &Path, to: &Path| {
                calls.set(calls.get() + 1);
                if allowed.contains(&calls.get()) {
                    fs::rename(from, to)
                } else {
                    Err(std::io::Error::other("refused"))
                }
            }
        };
        fake_bundle(&bundle, BUNDLE_ID, "0.1.0");
        fake_bundle(&new, BUNDLE_ID, "0.2.0");
        // The new bundle can't move in: the old one goes back.
        assert!(replace_bundle(&new, &bundle, &old, only(&[1, 3])).is_err());
        assert_eq!(version_of(&bundle), "0.1.0");
        // Nor can the old one go back: it is kept, and the error names it.
        let error = replace_bundle(&new, &bundle, &old, only(&[1])).unwrap_err();
        assert!(error.to_string().contains(&old.display().to_string()));
        assert_eq!(version_of(&old), "0.1.0");
        assert!(!bundle.exists());
        // Without failures the new bundle moves in and the old one is removed.
        fs::rename(&old, &bundle).unwrap();
        replace_bundle(&new, &bundle, &old, |from, to| fs::rename(from, to)).unwrap();
        assert_eq!(version_of(&bundle), "0.2.0");
        assert!(!old.exists() && !new.exists());
    }

    /// A disk image holding a signed RustTalk.app of `version`.
    #[cfg(target_os = "macos")]
    fn fake_dmg(dir: &Path, version: &str) -> PathBuf {
        let source = dir.join(format!("source-{version}"));
        signed_bundle(&source.join("RustTalk.app"), version);
        let dmg = dir.join(format!("update-{version}.dmg"));
        run(Command::new("/usr/bin/hdiutil")
            .args(["create", "-quiet", "-fs", "HFS+", "-format", "UDZO"])
            .args(["-volname", "RustTalk", "-srcfolder"])
            .arg(&source)
            .arg(&dmg))
        .unwrap();
        dmg
    }

    #[cfg(target_os = "macos")]
    fn installed(path: PathBuf) -> Target {
        Target {
            inode: file_id(&fs::symlink_metadata(&path).unwrap()),
            path,
        }
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn disk_image_replaces_the_installed_bundle() {
        let dir = tempfile::tempdir().unwrap();
        let dmg = fake_dmg(dir.path(), "0.2.0");
        let apps = dir.path().join("Applications");
        let bundle = apps.join("RustTalk.app");
        signed_bundle(&bundle, "0.1.0");
        let target = installed(bundle.clone());
        let cache = tempfile::tempdir().unwrap();
        // A download that doesn't match the signed version is refused.
        assert!(install_dmg(&dmg, &target, &Version::new(0, 3, 0), cache.path()).is_err());
        assert_eq!(version_of(&bundle), "0.1.0");
        install_dmg(&dmg, &target, &Version::new(0, 2, 0), cache.path()).unwrap();
        assert_eq!(version_of(&bundle), "0.2.0");
        // The staging folder and the old bundle are gone.
        assert_eq!(fs::read_dir(&apps).unwrap().count(), 1);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn a_stale_window_cant_install_over_a_newer_app() {
        let dir = tempfile::tempdir().unwrap();
        let dmg = fake_dmg(dir.path(), "0.2.0");
        let apps = dir.path().join("Applications");
        let bundle = apps.join("RustTalk.app");
        signed_bundle(&bundle, "0.1.0");
        let stale = installed(bundle.clone());
        // Another window replaced the bundle since this one started.
        fs::remove_dir_all(&bundle).unwrap();
        signed_bundle(&bundle, "0.1.0");
        let cache = tempfile::tempdir().unwrap();
        let error = install_dmg(&dmg, &stale, &Version::new(0, 2, 0), cache.path()).unwrap_err();
        assert!(error.to_string().contains("another window"));
        assert_eq!(version_of(&bundle), "0.1.0");
        assert_eq!(fs::read_dir(&apps).unwrap().count(), 1);
        // The lock file is closed on exec, but a child that another test thread is
        // spawning right now holds a copy of the descriptor until its exec finishes, and
        // the flock outlives our close until then. Wait that out instead of racing it.
        let free = || {
            let deadline = std::time::Instant::now() + Duration::from_secs(10);
            loop {
                match lock_bundle(cache.path(), &bundle) {
                    Ok(lock) => return lock,
                    Err(error) => assert!(std::time::Instant::now() < deadline, "{error:#}"),
                }
                std::thread::sleep(Duration::from_millis(50));
            }
        };
        // While one window installs, another can't start.
        let (held, _) = free();
        assert!(lock_bundle(cache.path(), &bundle).is_err());
        drop(held);
        drop(free());
    }

    /// Writes a minimal type 2 AppImage: an ELF64 header with the AppImage magic, no
    /// section headers, and a squashfs superblock right after it.
    #[cfg(target_os = "linux")]
    fn fake_appimage(path: &Path, inodes: u32, used: u64) {
        let mut bytes = vec![0u8; 64];
        bytes[..4].copy_from_slice(b"\x7fELF");
        bytes[4] = 2;
        bytes[5] = 1;
        bytes[8..11].copy_from_slice(b"AI\x02");
        bytes[0x28..0x30].copy_from_slice(&64u64.to_le_bytes());
        let mut superblock = [0u8; 96];
        superblock[..4].copy_from_slice(b"hsqs");
        superblock[4..8].copy_from_slice(&inodes.to_le_bytes());
        superblock[12..16].copy_from_slice(&131_072u32.to_le_bytes());
        superblock[40..48].copy_from_slice(&used.to_le_bytes());
        bytes.extend_from_slice(&superblock);
        fs::write(path, bytes).unwrap();
    }

    #[cfg(target_os = "linux")]
    fn mount_of(name: &str) -> Mount {
        Mount {
            fstype: format!("fuse.{name}"),
            source: name.to_owned(),
            block_size: 131_072,
            blocks: 3,
            files: 40,
        }
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn appimage_mount_is_found_by_escaped_mount_point() {
        let info = "431 252 0:84 / /tmp/.mount_T3-CodPgjFne ro,nosuid - fuse.T3-Code.AppImage T3-Code.AppImage ro\n\
                    1279 252 0:127 / /tmp/.mount_My\\040TesiakNcb ro,nosuid shared:911 - fuse.My\\040Test.AppImage My\\040Test.AppImage ro,user_id=1000\n";
        let mount = mount_at(info, Path::new("/tmp/.mount_My TesiakNcb")).unwrap();
        assert_eq!(mount.fstype, "fuse.My Test.AppImage");
        assert_eq!(mount.source, "My Test.AppImage");
        assert!(mount_at(info, Path::new("/tmp/.mount_other")).is_none());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn running_appimage_is_proven_by_its_mount() {
        let dir = tempfile::tempdir().unwrap();
        let image = dir.path().join("RustTalk.AppImage");
        fake_appimage(&image, 40, 300_000);
        prove_image(&image, &mount_of("RustTalk.AppImage")).unwrap();
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn inherited_appimage_pointing_at_another_appimage_is_refused() {
        // RustTalk runs from its own mount while APPIMAGE still names the AppImage of
        // the program that started it (for example an editor packaged as an AppImage).
        let dir = tempfile::tempdir().unwrap();
        let other = dir.path().join("T3-Code.AppImage");
        fake_appimage(&other, 40, 300_000);
        let before = fs::read(&other).unwrap();
        let ours = mount_of("RustTalk.AppImage");
        assert!(prove_image(&other, &ours).is_err());
        // Even an AppImage with the same file name is refused when its image differs.
        let renamed = dir.path().join("RustTalk.AppImage");
        fake_appimage(&renamed, 9_000, 200_000_000);
        assert!(prove_image(&renamed, &ours).is_err());
        // A plain file that is not an AppImage is refused too.
        let plain = dir.path().join("plain").join("RustTalk.AppImage");
        fs::create_dir(plain.parent().unwrap()).unwrap();
        fs::write(&plain, vec![0u8; 256]).unwrap();
        assert!(prove_image(&plain, &ours).is_err());
        // No process was started from the other AppImage, so it is not running either.
        assert!(!process_runs(file_id(&fs::metadata(&other).unwrap())));
        assert_eq!(fs::read(&other).unwrap(), before);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn running_program_files_are_detected() {
        let exe = env::current_exe().unwrap();
        assert!(process_runs(file_id(&fs::metadata(exe).unwrap())));
    }
}
