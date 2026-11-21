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
#[cfg(not(target_os = "macos"))]
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

#[cfg(target_os = "linux")]
static RESTART: std::sync::OnceLock<Target> = std::sync::OnceLock::new();

/// How often a running app looks for a new version.
const CHECK_EVERY: Duration = Duration::from_secs(6 * 60 * 60);

#[derive(Deserialize, Serialize)]
struct Settings {
    #[serde(default = "default_automatic")]
    automatic: bool,
    #[serde(default = "default_url")]
    url: String,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            automatic: default_automatic(),
            url: default_url(),
        }
    }
}

/// Release builds point at their own release directory.
fn default_url() -> String {
    option_env!("RUSTTALK_DEFAULT_UPDATE_URL")
        .unwrap_or_default()
        .to_owned()
}

/// Release builds check by default; a saved choice always wins over this.
fn default_automatic() -> bool {
    !default_url().is_empty()
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
    fn load(path: &Path) -> Result<Self> {
        crate::load_json(path)
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
    client: reqwest::Client,
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

async fn get(client: &reqwest::Client, url: Url) -> Result<reqwest::Response> {
    let response = client
        .get(url)
        .send()
        .await
        .map_err(reqwest::Error::without_url)?;
    ensure!(
        response.status() == reqwest::StatusCode::OK,
        "update host returned HTTP {}",
        response.status()
    );
    Ok(response)
}

async fn read_small(client: &reqwest::Client, url: Url, limit: usize) -> Result<Vec<u8>> {
    let mut response = get(client, url).await?;
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

async fn check(url: &str) -> Result<Option<Update>> {
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
    let client = reqwest::Client::builder()
        .no_proxy()
        .https_only(!local_http)
        // Release hosts such as GitHub redirect downloads to a CDN. The signed
        // manifest pins every byte, so only the scheme needs checking here.
        .redirect(reqwest::redirect::Policy::custom(move |attempt| {
            if attempt.previous().len() < 5 && (attempt.url().scheme() == "https" || local_http) {
                attempt.follow()
            } else {
                attempt.stop()
            }
        }))
        .connect_timeout(Duration::from_secs(10))
        .timeout(Duration::from_secs(300))
        .build()?;
    let bytes = read_small(&client, base.join("latest.json")?, 16 * 1024).await?;
    let signature = read_small(&client, base.join("latest.json.sig")?, 64).await?;
    let mut manifest = verify(&key, &bytes, &signature)?;
    let version = Version::parse(&manifest.version)?;
    if !newer(&version, &Version::parse(env!("CARGO_PKG_VERSION"))?)? {
        return Ok(None);
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
    Ok(Some(Update {
        client,
        base,
        version,
        artifact,
    }))
}

#[derive(Clone)]
struct Target {
    path: PathBuf,
    #[cfg(target_os = "linux")]
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
        #[cfg(target_os = "macos")]
        {
            let _ = config;
            anyhow::bail!("in-app updates are not available on macOS yet")
        }
    }

    async fn install(self, update: Update, done: Arc<AtomicU64>) -> Result<State> {
        let mut file =
            NamedTempFile::new_in(self.path.parent().context("locate update directory")?)?;
        let mut response = get(&update.client, update.base.join(&update.artifact.file)?).await?;
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
            let _ = (file, update);
            anyhow::bail!("in-app updates are not available on macOS yet")
        }
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
    #[cfg(target_os = "linux")]
    if let Some(target) = RESTART.get() {
        target.restart()?;
    }
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

#[cfg(target_os = "linux")]
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
        if let Some(update) = check(&url).await? {
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
    Current,
    #[cfg(target_os = "linux")]
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
        }
    }

    fn start_check(&mut self, ctx: &egui::Context) {
        let url = self.settings.url.clone();
        self.state = background(ctx, None, move || {
            Ok(runtime()?
                .block_on(check(&url))?
                .map_or(State::Current, State::Available))
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
            #[cfg(target_os = "linux")]
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
        #[cfg(target_os = "linux")]
        if let State::Installed(_, target) = &self.state {
            let _ = RESTART.set(target.clone());
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
        }
        // The installer restarts RustTalk itself once it is closed.
        #[cfg(target_os = "windows")]
        if matches!(self.state, State::InstallerStarted(_)) {
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
        }
        let ready = self.settings.automatic
            && !self.settings.url.trim().is_empty()
            && self.target.is_ok()
            && !self.busy()
            && !self.asking
            && !self.available();
        if ready {
            let now = Instant::now();
            if check_due(self.next_check, now) {
                self.next_check = Some(now + CHECK_EVERY);
                self.start_check(ctx);
            } else if let Some(next) = self.next_check {
                ctx.request_repaint_after(next.saturating_duration_since(now));
            }
        }
    }

    pub fn available(&self) -> bool {
        matches!(self.state, State::Available(_))
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
            let changed = crate::ui::toggle_row(
                ui,
                &mut self.settings.automatic,
                t!("update.automatic"),
                t!("update.automatic_help"),
            )
            .changed();
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
            if url.changed() && self.available() {
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
            State::Current => {
                ui.label(t!("update.up_to_date"));
            }
            #[cfg(target_os = "linux")]
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
            client: reqwest::Client::new(),
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
            },
            target: Err(anyhow!("no target in tests")),
            state,
            next_check: None,
            asking: false,
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
    fn schedule_checks_at_startup_then_every_six_hours() {
        let start = Instant::now();
        assert!(check_due(None, start));
        let next = Some(start + CHECK_EVERY);
        assert!(!check_due(
            next,
            start + CHECK_EVERY - Duration::from_secs(1)
        ));
        assert!(check_due(next, start + CHECK_EVERY));
        assert_eq!(CHECK_EVERY, Duration::from_secs(21_600));
    }

    #[test]
    fn poll_starts_one_scheduled_check_and_respects_the_switch() {
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
            #[cfg(target_os = "linux")]
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
    fn update_flow_goes_from_available_to_failed() {
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
