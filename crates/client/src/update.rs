use std::{
    env, fs,
    io::Write,
    path::{Path, PathBuf},
    process::Command,
    sync::mpsc::{self, Receiver, TryRecvError},
    time::Duration,
};

use anyhow::{Context, Result, anyhow, ensure};
use ed25519_dalek::{Signature, VerifyingKey};
use eframe::egui;
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

#[derive(Default, Deserialize, Serialize)]
struct Settings {
    #[serde(default)]
    automatic: bool,
    #[serde(default)]
    url: String,
}

impl Settings {
    fn load(path: &Path) -> Result<Self> {
        match fs::read(path) {
            Ok(bytes) => Ok(serde_json::from_slice(&bytes)?),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(error) => Err(error.into()),
        }
    }

    fn save(&self, path: &Path) -> Result<()> {
        let mut file = NamedTempFile::new_in(path.parent().context("locate update settings")?)?;
        serde_json::to_writer(&mut file, self)?;
        file.persist(path)?;
        Ok(())
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
        .redirect(reqwest::redirect::Policy::none())
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
            // Other AppImages can leave APPDIR and APPIMAGE in a child process's environment.
            let running_appdir = env::var_os("APPDIR").is_some_and(|dir| {
                fs::canonicalize(PathBuf::from(dir).join("usr/bin/rusttalk-client"))
                    .is_ok_and(|client| client == executable)
            });
            let path = if running_appdir {
                let image = env::var_os("APPIMAGE").context("locate running AppImage")?;
                let path = fs::canonicalize(image).context("locate running AppImage")?;
                let mut header = [0u8; 11];
                std::io::Read::read_exact(&mut fs::File::open(&path)?, &mut header)?;
                ensure!(
                    &header[..4] == b"\x7fELF" && &header[8..] == b"AI\x02",
                    "invalid AppImage target"
                );
                path
            } else {
                ensure!(
                    LOCAL_TEST,
                    "install the RustTalk AppImage to use in-app updates"
                );
                executable
            };
            Ok(Self {
                inode: file_id(&fs::metadata(&path)?),
                path,
            })
        }
        #[cfg(target_os = "windows")]
        {
            Ok(Self {
                path: config.join("rusttalk-update.exe"),
            })
        }
    }

    async fn install(self, update: Update) -> Result<State> {
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
            target.install(update).await?;
            println!("Installed RustTalk {version}");
        } else {
            println!("RustTalk is up to date");
        }
        Ok(())
    })
}

enum State {
    Idle,
    Working(Receiver<Result<State>>),
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
}

impl Updater {
    pub fn new(config: &Path, ctx: &egui::Context) -> Self {
        let path = config.join("updates.json");
        let (settings, state) = match Settings::load(&path) {
            Ok(settings) => (settings, State::Idle),
            Err(error) => (
                Settings::default(),
                failure(error.context("load update settings")),
            ),
        };
        let mut updater = Self {
            config: path,
            settings,
            target: Target::new(config),
            state,
        };
        if updater.settings.automatic
            && !updater.settings.url.trim().is_empty()
            && updater.target.is_ok()
        {
            updater.start_check(ctx);
        }
        updater
    }

    fn start_check(&mut self, ctx: &egui::Context) {
        let url = self.settings.url.clone();
        self.state = background(ctx, move || {
            Ok(runtime()?
                .block_on(check(&url))?
                .map_or(State::Current, State::Available))
        });
    }

    pub fn poll(&mut self, _ctx: &egui::Context) {
        if let State::Working(receiver) = &self.state {
            match receiver.try_recv() {
                Ok(result) => self.state = result.unwrap_or_else(failure),
                Err(TryRecvError::Disconnected) => {
                    self.state = failure(anyhow!("update worker stopped"))
                }
                Err(TryRecvError::Empty) => {}
            }
        }
        #[cfg(target_os = "windows")]
        if matches!(self.state, State::InstallerStarted(_)) {
            _ctx.send_viewport_cmd(egui::ViewportCommand::Close);
        }
    }

    pub fn available(&self) -> bool {
        matches!(self.state, State::Available(_))
    }

    pub fn settings(&mut self, ui: &mut egui::Ui) {
        let can_check = match self.state {
            State::Working(_) => false,
            #[cfg(target_os = "linux")]
            State::Installed(..) => false,
            _ => true,
        };
        ui.add_enabled_ui(can_check, |ui| {
            let changed = ui
                .checkbox(
                    &mut self.settings.automatic,
                    "Check for updates automatically",
                )
                .changed();
            let url = ui.add(
                egui::TextEdit::singleline(&mut self.settings.url).hint_text("HTTPS update URL"),
            );
            if url.changed() && self.available() {
                self.state = State::Idle;
            }
            let check = ui
                .add_enabled(
                    self.target.is_ok() && !self.settings.url.trim().is_empty(),
                    egui::Button::new("Check now"),
                )
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
            State::Working(_) => {
                ui.spinner();
            }
            State::Available(update) => {
                if ui
                    .button(format!("Install RustTalk {}", update.version))
                    .clicked()
                    && let State::Available(update) =
                        std::mem::replace(&mut self.state, State::Idle)
                    && let Ok(target) = &self.target
                {
                    let target = target.clone();
                    self.state = background(ui.ctx(), move || {
                        runtime()?.block_on(target.install(update))
                    });
                }
            }
            State::Current => {
                ui.label("RustTalk is up to date.");
            }
            #[cfg(target_os = "linux")]
            State::Installed(version, target) => {
                ui.label(format!("RustTalk {version} installed."));
                if ui.button("Restart now").clicked() {
                    let _ = RESTART.set(target.clone());
                    ui.ctx().send_viewport_cmd(egui::ViewportCommand::Close);
                }
            }
            #[cfg(target_os = "windows")]
            State::InstallerStarted(version) => {
                ui.label(format!("Starting RustTalk {version} installer."));
            }
            State::Failed(error) => {
                ui.label(error);
            }
        }
    }
}

fn background(ctx: &egui::Context, work: impl FnOnce() -> Result<State> + Send + 'static) -> State {
    let (send, receive) = mpsc::channel();
    let ctx = ctx.clone();
    match std::thread::Builder::new()
        .name("rusttalk-update".into())
        .spawn(move || {
            let _ = send.send(work());
            ctx.request_repaint();
        }) {
        Ok(_) => State::Working(receive),
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

    #[test]
    fn only_a_newer_release_can_install() {
        let current = Version::new(0, 1, 1);
        assert!(newer(&Version::new(0, 1, 2), &current).unwrap());
        assert!(!newer(&current, &current).unwrap());
        assert!(newer(&Version::new(0, 1, 0), &current).is_err());
    }
}
