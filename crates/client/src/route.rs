//! Per-device network routing: persistence, secure credential storage across all five
//! platforms, cold-start enforcement, and the shared onboarding + Settings UI.

use std::{
    fs,
    io::ErrorKind,
    path::{Path, PathBuf},
    sync::{Mutex, OnceLock, mpsc},
    time::Duration,
};

use anyhow::{Context, Result};
use eframe::egui::{
    self, Align, CollapsingHeader, Frame, Id, Layout, Margin, RichText, Stroke, Ui, WidgetInfo,
    accesskit::Role, text_edit::TextEditState,
};
use rusttalk_core::{
    client::save_json,
    route::{
        self, RouteConfig, RouteError, RouteKind, RouteSecrets, RouteSnapshot, RouteStatus,
        WireGuardConfig,
    },
};
use zeroize::Zeroizing;

use crate::{
    audio::lock,
    key_storage::secret_input,
    ui::{self, CONTROL_HEIGHT, SPACE_M, SPACE_S, SPACE_XS, error_text},
};

/// Non-secret route configuration file in the profile folder (never synced).
pub const FILE: &str = "route.json";
/// Fallback user-only secrets file on desktops without an OS keychain (e.g. Steam Deck).
#[cfg(not(any(target_os = "android", target_os = "ios")))]
const FALLBACK_SECRET_FILE: &str = "route.secret";
/// Android Keystore AES-GCM wrapped secrets blob in private app storage.
#[cfg(target_os = "android")]
const WRAPPED_SECRET_FILE: &str = "route.wrapped";
const KEYCHAIN_ACCOUNT: &str = "route-secrets-v1";
const MAX_FIELD_WIDTH: f32 = 460.0;
const POLL: Duration = Duration::from_millis(80);

/// Account files that indicate an existing profile created before per-app routing existed.
const EXISTING_PROFILE_FILES: [&str; 3] = ["servers.json", "sync.json", "keystore.json"];

#[derive(Clone)]
struct StoreContext {
    data: PathBuf,
    service: String,
}

static STORE: OnceLock<Mutex<StoreContext>> = OnceLock::new();
/// In-memory session fallback when a phone keychain is unavailable (never written plain).
static SESSION_SECRETS: Mutex<Option<RouteSecrets>> = Mutex::new(None);

fn store_ctx() -> Option<StoreContext> {
    STORE.get().map(|m| lock(m).clone())
}

fn set_store_ctx(data: &Path, service: &str) {
    let ctx = StoreContext {
        data: data.to_path_buf(),
        service: service.to_owned(),
    };
    if let Some(cell) = STORE.get() {
        *lock(cell) = ctx;
    } else {
        let _ = STORE.set(Mutex::new(ctx));
    }
}

/// Result of cold-start route initialization.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum InitState {
    /// Fresh profile: show the onboarding network-route choice before any network traffic.
    Onboarding,
    /// Existing profile or saved route already applied/connecting.
    Ready,
}

/// Initialize routing at process startup BEFORE any background task or update check starts.
pub fn init(data: &Path, service: &str) -> InitState {
    set_store_ctx(data, service);
    let path = data.join(FILE);
    if !path.exists() {
        let existing_profile = EXISTING_PROFILE_FILES
            .iter()
            .any(|name| data.join(name).exists());
        if existing_profile {
            let _ = route::apply(RouteConfig::default(), RouteSecrets::default());
            return InitState::Ready;
        }
        route::enter_pending();
        return InitState::Onboarding;
    }

    match load_config(&path) {
        Ok(config) => {
            if config.kind.is_direct() {
                let _ = route::apply(config, RouteSecrets::default());
                return InitState::Ready;
            }
            let secrets = match load_secrets(data, service) {
                Ok(secrets) => secrets,
                Err(error) => {
                    tracing::warn!(error = %format_args!("{error:#}"), "loading route secrets failed");
                    route::set_blocked(config, t!("route.secrets_unavailable"));
                    return InitState::Ready;
                }
            };
            route::set_connecting(config.clone());
            // Activate in the background so startup never blocks the UI thread; all
            // network tasks stay blocked while Connecting and fail closed on error.
            let _ = std::thread::Builder::new()
                .name("rusttalk-route-init".into())
                .spawn(move || {
                    let _ = route::apply(config, secrets);
                });
            InitState::Ready
        }
        Err(error) => {
            tracing::error!(error = %format_args!("{error:#}"), "reading route.json failed");
            route::set_blocked(RouteConfig::default(), t!("route.config_unreadable"));
            InitState::Ready
        }
    }
}

/// Blocking cold-start initialization for headless CLI commands (e.g. `--check-update-now`).
#[cfg(any(target_os = "linux", test))]
pub fn init_blocking(data: &Path, service: &str) -> Result<()> {
    set_store_ctx(data, service);
    let path = data.join(FILE);
    if !path.exists() {
        let existing_profile = EXISTING_PROFILE_FILES
            .iter()
            .any(|name| data.join(name).exists());
        if existing_profile {
            let _ = route::apply(RouteConfig::default(), RouteSecrets::default());
            return Ok(());
        }
        route::enter_pending();
        return Err(anyhow::anyhow!(
            "choose a network route during onboarding before connecting"
        ));
    }
    let config = match load_config(&path) {
        Ok(config) => config,
        Err(error) => {
            tracing::error!(error = %format_args!("{error:#}"), "reading route.json failed");
            route::set_blocked(RouteConfig::default(), t!("route.config_unreadable"));
            return Err(error.context("read saved network route"));
        }
    };
    if config.kind.is_direct() {
        let _ = route::apply(config, RouteSecrets::default());
        return Ok(());
    }
    let secrets = match load_secrets(data, service) {
        Ok(secrets) => secrets,
        Err(error) => {
            tracing::warn!(error = %format_args!("{error:#}"), "loading route secrets failed");
            route::set_blocked(config, t!("route.secrets_unavailable"));
            return Err(error.context("load saved network route credentials"));
        }
    };
    route::apply(config, secrets).map_err(|e| anyhow::anyhow!("{e}"))?;
    Ok(())
}

/// Apply the active route to a blocking `reqwest::blocking::ClientBuilder`.
/// Always disables OS/env proxies (`.no_proxy()`), installs the active `socks5h://`
/// proxy when routed, and fails closed when pending/connecting/blocked.
pub fn configure_blocking_reqwest(
    builder: reqwest::blocking::ClientBuilder,
) -> Result<reqwest::blocking::ClientBuilder, RouteError> {
    let builder = builder.no_proxy();
    match route::http_proxy()? {
        None => Ok(builder),
        Some(proxy) => Ok(builder.proxy(proxy)),
    }
}

/// Apply the active route to a blocking `reqwest::blocking::ClientBuilder` for a
/// destination whose IP addresses were already resolved and SSRF-validated.
/// Installs a strict `PinnedDnsResolver` (zero OS DNS, zero re-resolution) and,
/// for WireGuard, uses the `socks5://` loopback bridge so `reqwest` sends the
/// pre-validated IP literal to the tunnel while keeping the original hostname for
/// the HTTP `Host` header and TLS SNI.
pub fn configure_blocking_reqwest_resolved(
    builder: reqwest::blocking::ClientBuilder,
    host: &str,
    addrs: Vec<std::net::SocketAddr>,
) -> Result<reqwest::blocking::ClientBuilder, RouteError> {
    let resolver = std::sync::Arc::new(route::PinnedDnsResolver::new(host, addrs));
    let builder = builder.no_proxy().dns_resolver(resolver);
    match route::http_proxy_for_resolved()? {
        None => Ok(builder),
        Some(proxy) => Ok(builder.proxy(proxy)),
    }
}

fn load_config(path: &Path) -> Result<RouteConfig> {
    let bytes = fs::read(path).with_context(|| format!("open {}", path.display()))?;
    serde_json::from_slice(&bytes).with_context(|| format!("parse {}", path.display()))
}

fn save_config(data: &Path, config: &RouteConfig) -> Result<()> {
    save_json(data, &data.join(FILE), config).context("save route.json")
}

/// Where route secrets were persisted.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SecretStorage {
    Keychain,
    /// Desktop without an OS keychain (mode 0600 file).
    PrivateFile,
    /// Phone without an available keychain/keystore (in memory only until restart).
    MemoryOnly,
}

fn load_secrets(data: &Path, service: &str) -> Result<RouteSecrets> {
    if let Some(mem) = lock(&SESSION_SECRETS).clone() {
        return Ok(mem);
    }
    platform_secrets::load(data, service)
}

fn save_secrets(data: &Path, service: &str, secrets: &RouteSecrets) -> Result<SecretStorage> {
    if secrets.is_empty() {
        *lock(&SESSION_SECRETS) = None;
        let _ = platform_secrets::delete(data, service);
        return Ok(SecretStorage::Keychain);
    }
    match platform_secrets::save(data, service, secrets) {
        Ok(where_saved) => {
            *lock(&SESSION_SECRETS) = None;
            Ok(where_saved)
        }
        Err(error) => {
            tracing::warn!(error = %format_args!("{error:#}"), "saving route secrets to secure storage failed");
            *lock(&SESSION_SECRETS) = Some(secrets.clone());
            Ok(SecretStorage::MemoryOnly)
        }
    }
}

/// Platform-specific secure storage for route credentials and WireGuard keys.
mod platform_secrets {
    use super::*;

    #[cfg(not(any(target_os = "android", target_os = "ios")))]
    pub(super) fn load(data: &Path, service: &str) -> Result<RouteSecrets> {
        use keyring::{Entry, Error};
        if let Ok(entry) = Entry::new(service, KEYCHAIN_ACCOUNT) {
            match entry.get_secret() {
                Ok(bytes) => {
                    let bytes = Zeroizing::new(bytes);
                    return serde_json::from_slice(&bytes)
                        .context("decode route secrets from OS keychain");
                }
                Err(Error::NoEntry) => {}
                Err(error) => {
                    tracing::warn!(%error, "reading route secrets from OS keychain failed");
                }
            }
        }
        let fallback = data.join(FALLBACK_SECRET_FILE);
        match fs::read(&fallback) {
            Ok(bytes) => {
                let bytes = Zeroizing::new(bytes);
                serde_json::from_slice(&bytes).context("decode route secrets from private file")
            }
            Err(e) if e.kind() == ErrorKind::NotFound => Ok(RouteSecrets::default()),
            Err(e) => Err(e).context("read route secrets file"),
        }
    }

    #[cfg(not(any(target_os = "android", target_os = "ios")))]
    pub(super) fn save(
        data: &Path,
        service: &str,
        secrets: &RouteSecrets,
    ) -> Result<SecretStorage> {
        use keyring::Entry;
        let bytes = Zeroizing::new(serde_json::to_vec(secrets).context("encode route secrets")?);
        let fallback = data.join(FALLBACK_SECRET_FILE);
        if Entry::store_status().is_ok()
            && let Ok(entry) = Entry::new(service, KEYCHAIN_ACCOUNT)
            && entry.set_secret(&bytes).is_ok()
        {
            let _ = fs::remove_file(&fallback);
            return Ok(SecretStorage::Keychain);
        }
        write_private_file(data, &fallback, &bytes)?;
        Ok(SecretStorage::PrivateFile)
    }

    #[cfg(not(any(target_os = "android", target_os = "ios")))]
    pub(super) fn delete(data: &Path, service: &str) -> Result<()> {
        use keyring::{Entry, Error};
        if let Ok(entry) = Entry::new(service, KEYCHAIN_ACCOUNT) {
            match entry.delete_credential() {
                Ok(()) | Err(Error::NoEntry) => {}
                Err(error) => {
                    tracing::warn!(%error, "removing route secrets from OS keychain failed");
                }
            }
        }
        let fallback = data.join(FALLBACK_SECRET_FILE);
        if let Err(e) = fs::remove_file(&fallback)
            && e.kind() != ErrorKind::NotFound
        {
            tracing::warn!(%e, "removing fallback route secrets file failed");
        }
        Ok(())
    }

    #[cfg(not(any(target_os = "android", target_os = "ios")))]
    fn write_private_file(dir: &Path, path: &Path, bytes: &[u8]) -> Result<()> {
        use std::io::Write;
        let mut temp = tempfile::NamedTempFile::new_in(dir)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            temp.as_file()
                .set_permissions(fs::Permissions::from_mode(0o600))?;
        }
        temp.write_all(bytes)?;
        temp.as_file().sync_all()?;
        temp.persist(path).map_err(|e| e.error)?;
        Ok(())
    }

    #[cfg(target_os = "ios")]
    fn ios_entry(service: &str) -> Result<keyring::Entry> {
        use apple_native_keyring_store::protected::{AccessPolicy, Cred};
        let inner = Cred::build(
            service,
            KEYCHAIN_ACCOUNT,
            AccessPolicy::WhenUnlockedThisDeviceOnly,
            None,
            false,
        )
        .map_err(|e| anyhow::anyhow!("open iOS Keychain for route secrets: {e}"))?;
        Ok(keyring::Entry { inner })
    }

    #[cfg(target_os = "ios")]
    pub(super) fn load(_data: &Path, service: &str) -> Result<RouteSecrets> {
        match ios_entry(service)?.get_secret() {
            Ok(bytes) => {
                let bytes = Zeroizing::new(bytes);
                serde_json::from_slice(&bytes).context("decode route secrets from iOS Keychain")
            }
            Err(keyring::Error::NoEntry) => Ok(RouteSecrets::default()),
            Err(e) => Err(anyhow::anyhow!("read route secrets from iOS Keychain: {e}")),
        }
    }

    #[cfg(target_os = "ios")]
    pub(super) fn save(
        _data: &Path,
        service: &str,
        secrets: &RouteSecrets,
    ) -> Result<SecretStorage> {
        let bytes = Zeroizing::new(serde_json::to_vec(secrets).context("encode route secrets")?);
        ios_entry(service)?
            .set_secret(&bytes)
            .map_err(|e| anyhow::anyhow!("save route secrets to iOS Keychain: {e}"))?;
        Ok(SecretStorage::Keychain)
    }

    #[cfg(target_os = "ios")]
    pub(super) fn delete(_data: &Path, service: &str) -> Result<()> {
        match ios_entry(service)?.delete_credential() {
            Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
            Err(e) => Err(anyhow::anyhow!(
                "delete route secrets from iOS Keychain: {e}"
            )),
        }
    }

    #[cfg(target_os = "android")]
    fn android_crypt(service: &str, encrypt: bool, bytes: &[u8]) -> Result<Zeroizing<Vec<u8>>> {
        use jni::{JValue, jni_sig, jni_str, objects::JByteArray};
        let app = crate::android::app().context("no Android activity")?;
        let alias_str = format!("{service}/{KEYCHAIN_ACCOUNT}");
        crate::android::java(app, |env, activity| {
            let alias = env.new_string(&alias_str)?;
            let input = env.byte_array_from_slice(bytes)?;
            let output = env
                .call_method(
                    activity,
                    jni_str!("cryptIdentity"),
                    jni_sig!("(Ljava/lang/String;Z[B)[B"),
                    &[
                        JValue::Object(&alias),
                        JValue::Bool(encrypt),
                        JValue::Object(&input),
                    ],
                )?
                .l()?;
            let output = env.cast_local::<JByteArray>(output)?;
            let result = Zeroizing::new(env.convert_byte_array(&output)?);
            if !encrypt {
                output.set_region(env, 0, &vec![0; result.len()])?;
            }
            Ok(result)
        })
        .context("Android Keystore route secrets operation")
    }

    #[cfg(target_os = "android")]
    pub(super) fn load(data: &Path, service: &str) -> Result<RouteSecrets> {
        let path = data.join(WRAPPED_SECRET_FILE);
        match fs::read(&path) {
            Ok(blob) => {
                let plain = android_crypt(service, false, &blob)?;
                serde_json::from_slice(&plain).context("decode wrapped Android route secrets")
            }
            Err(e) if e.kind() == ErrorKind::NotFound => Ok(RouteSecrets::default()),
            Err(e) => Err(e).context("read wrapped Android route secrets"),
        }
    }

    #[cfg(target_os = "android")]
    pub(super) fn save(
        data: &Path,
        service: &str,
        secrets: &RouteSecrets,
    ) -> Result<SecretStorage> {
        use std::io::Write;
        let bytes = Zeroizing::new(serde_json::to_vec(secrets).context("encode route secrets")?);
        let wrapped = android_crypt(service, true, &bytes)?;
        let path = data.join(WRAPPED_SECRET_FILE);
        let mut temp = tempfile::NamedTempFile::new_in(data)?;
        temp.write_all(&wrapped)?;
        temp.as_file().sync_all()?;
        temp.persist(&path).map_err(|e| e.error)?;
        Ok(SecretStorage::Keychain)
    }

    #[cfg(target_os = "android")]
    pub(super) fn delete(data: &Path, service: &str) -> Result<()> {
        use jni::{JValue, jni_sig, jni_str};
        if let Some(app) = crate::android::app() {
            let alias_str = format!("{service}/{KEYCHAIN_ACCOUNT}");
            let _ = crate::android::java(app, |env, activity| {
                let alias = env.new_string(&alias_str)?;
                env.call_method(
                    activity,
                    jni_str!("deleteIdentityKey"),
                    jni_sig!("(Ljava/lang/String;)V"),
                    &[JValue::Object(&alias)],
                )?
                .v()
            });
        }
        let path = data.join(WRAPPED_SECRET_FILE);
        if let Err(e) = fs::remove_file(&path)
            && e.kind() != ErrorKind::NotFound
        {
            tracing::warn!(%e, "removing wrapped Android route secrets failed");
        }
        Ok(())
    }
}

/// Secret field IDs so `forget()` clears egui's undo history and cached layout state.
fn secret_field_id(name: &str) -> Id {
    Id::unique(("route secret", name))
}

const SECRET_FIELDS: [&str; 5] = ["socks5-pass", "tor-pass", "wg-priv", "wg-psk", "wg-conf"];

pub fn forget(ctx: &egui::Context) {
    ctx.data_mut(|data| {
        for name in SECRET_FIELDS {
            data.remove::<TextEditState>(secret_field_id(name));
        }
    });
}

enum JobOutcome {
    Test(Result<String, String>),
    Apply {
        config: Box<RouteConfig>,
        result: Result<String, String>,
        save_error: Option<String>,
        notice: Option<String>,
    },
}

struct PendingJob {
    done: mpsc::Receiver<JobOutcome>,
}

/// Interactive form state shared between onboarding (`Screen::Network`) and Settings (`Tab::Network`).
pub struct RouteEditor {
    draft: RouteConfig,
    socks5_port: String,
    tor_port: String,
    wg_addresses: String,
    wg_dns: String,
    wg_allowed_ips: String,
    wg_mtu: String,
    wg_keepalive: String,
    socks5_password: Zeroizing<String>,
    tor_password: Zeroizing<String>,
    wg_private_key: Zeroizing<String>,
    wg_preshared_key: Zeroizing<String>,
    wg_conf: Zeroizing<String>,
    clear_socks5_password: bool,
    clear_tor_password: bool,
    clear_wg_preshared_key: bool,
    job: Option<PendingJob>,
    test_ok: Option<String>,
    error: Option<String>,
    notice: Option<String>,
    synced_generation: u64,
}

impl Default for RouteEditor {
    fn default() -> Self {
        let snap = route::snapshot();
        Self::from_config(snap.config, snap.generation)
    }
}

impl RouteEditor {
    pub fn from_config(config: RouteConfig, generation: u64) -> Self {
        Self {
            socks5_port: config.socks5.port.to_string(),
            tor_port: config.tor.port.to_string(),
            wg_addresses: config.wireguard.addresses.join(", "),
            wg_dns: config.wireguard.dns.join(", "),
            wg_allowed_ips: config.wireguard.allowed_ips.join(", "),
            wg_mtu: config.wireguard.mtu.to_string(),
            wg_keepalive: config
                .wireguard
                .persistent_keepalive
                .map_or_else(String::new, |s| s.to_string()),
            draft: config,
            socks5_password: secret_input(),
            tor_password: secret_input(),
            wg_private_key: secret_input(),
            wg_preshared_key: secret_input(),
            wg_conf: Zeroizing::new(String::new()),
            clear_socks5_password: false,
            clear_tor_password: false,
            clear_wg_preshared_key: false,
            job: None,
            test_ok: None,
            error: None,
            notice: None,
            synced_generation: generation,
        }
    }

    pub fn sync_if_idle(&mut self) {
        let snap = route::snapshot();
        if self.job.is_none() && self.synced_generation != snap.generation {
            self.synced_generation = snap.generation;
        }
    }

    fn build_candidate(&self) -> Result<(RouteConfig, RouteSecrets), String> {
        let mut cfg = self.draft.clone();
        cfg.socks5.host = cfg.socks5.host.trim().to_owned();
        cfg.socks5.username = cfg.socks5.username.trim().to_owned();
        cfg.tor.host = cfg.tor.host.trim().to_owned();
        cfg.tor.username = cfg.tor.username.trim().to_owned();
        cfg.wireguard.peer_public_key = cfg.wireguard.peer_public_key.trim().to_owned();
        cfg.wireguard.endpoint = cfg.wireguard.endpoint.trim().to_owned();

        if cfg.kind == RouteKind::Socks5 {
            cfg.socks5.port = self
                .socks5_port
                .trim()
                .parse::<u16>()
                .ok()
                .filter(|&p| p > 0)
                .ok_or_else(|| t!("route.bad_port").to_owned())?;
        }
        if cfg.kind == RouteKind::Tor {
            cfg.tor.port = self
                .tor_port
                .trim()
                .parse::<u16>()
                .ok()
                .filter(|&p| p > 0)
                .ok_or_else(|| t!("route.bad_port").to_owned())?;
        }
        if cfg.kind == RouteKind::WireGuard {
            cfg.wireguard.addresses = split_csv(&self.wg_addresses);
            cfg.wireguard.dns = split_csv(&self.wg_dns);
            cfg.wireguard.allowed_ips = split_csv(&self.wg_allowed_ips);
            cfg.wireguard.mtu = if self.wg_mtu.trim().is_empty() {
                1280
            } else {
                self.wg_mtu
                    .trim()
                    .parse::<u16>()
                    .map_err(|_| t!("route.bad_mtu").to_owned())?
                    .clamp(576, 1500)
            };
            cfg.wireguard.persistent_keepalive = if self.wg_keepalive.trim().is_empty() {
                None
            } else {
                let secs = self
                    .wg_keepalive
                    .trim()
                    .parse::<u16>()
                    .map_err(|_| t!("route.bad_keepalive").to_owned())?;
                (secs > 0).then_some(secs)
            };
        }

        // Start from existing stored secrets so leaving a password field blank keeps
        // the saved credential unless explicitly cleared.
        let mut secrets = store_ctx()
            .and_then(|ctx| load_secrets(&ctx.data, &ctx.service).ok())
            .unwrap_or_default();

        if self.clear_socks5_password {
            secrets.socks5_password = None;
            cfg.socks5.has_password = false;
        } else if !self.socks5_password.is_empty() {
            secrets.socks5_password = Some(self.socks5_password.clone());
            cfg.socks5.has_password = true;
        } else {
            cfg.socks5.has_password = secrets
                .socks5_password
                .as_deref()
                .is_some_and(|s| !s.is_empty());
        }

        if self.clear_tor_password {
            secrets.tor_password = None;
            cfg.tor.has_password = false;
        } else if !self.tor_password.is_empty() {
            secrets.tor_password = Some(self.tor_password.clone());
            cfg.tor.has_password = true;
        } else {
            cfg.tor.has_password = secrets
                .tor_password
                .as_deref()
                .is_some_and(|s| !s.is_empty());
        }

        if !self.wg_private_key.trim().is_empty() {
            secrets.wg_private_key = Some(Zeroizing::new(self.wg_private_key.trim().to_owned()));
            cfg.wireguard.has_private_key = true;
        } else {
            cfg.wireguard.has_private_key = secrets
                .wg_private_key
                .as_deref()
                .is_some_and(|s| !s.is_empty());
        }

        if self.clear_wg_preshared_key {
            secrets.wg_preshared_key = None;
            cfg.wireguard.has_preshared_key = false;
        } else if !self.wg_preshared_key.trim().is_empty() {
            secrets.wg_preshared_key =
                Some(Zeroizing::new(self.wg_preshared_key.trim().to_owned()));
            cfg.wireguard.has_preshared_key = true;
        } else {
            cfg.wireguard.has_preshared_key = secrets
                .wg_preshared_key
                .as_deref()
                .is_some_and(|s| !s.is_empty());
        }

        route::validate(&cfg, &secrets).map_err(|e| e.to_string())?;
        Ok((cfg, secrets))
    }

    fn start_test(&mut self, ctx: &egui::Context) {
        self.error = None;
        self.test_ok = None;
        self.notice = None;
        let (cfg, secrets) = match self.build_candidate() {
            Ok(pair) => pair,
            Err(err) => {
                self.error = Some(err);
                return;
            }
        };
        let (tx, rx) = mpsc::channel();
        let egui = ctx.clone();
        let _ = std::thread::Builder::new()
            .name("rusttalk-route-test".into())
            .spawn(move || {
                let res = route::test_route(&cfg, &secrets).map_err(|e| e.to_string());
                let _ = tx.send(JobOutcome::Test(res));
                egui.request_repaint();
            });
        self.job = Some(PendingJob { done: rx });
    }

    fn start_apply(&mut self, ctx: &egui::Context, cfg: RouteConfig, secrets: RouteSecrets) {
        self.error = None;
        self.test_ok = None;
        self.notice = None;
        let cfg_bg = cfg.clone();
        let secrets_bg = secrets.clone();
        let store = store_ctx();
        let (tx, rx) = mpsc::channel();
        let egui = ctx.clone();
        let _ = std::thread::Builder::new()
            .name("rusttalk-route-apply".into())
            .spawn(move || {
                // Persist the user's explicit route choice inside the worker so leaving
                // Settings -> Network (or closing the app) mid-handshake never drops it
                // or silently reverts to Direct on restart.
                let mut save_error = None;
                let mut notice = None;
                if let Some(store) = store {
                    if let Err(err) = save_config(&store.data, &cfg_bg) {
                        tracing::error!(error = %format_args!("{err:#}"), "saving route.json failed");
                        save_error = Some(t!("route.save_failed").to_owned());
                    } else {
                        match save_secrets(&store.data, &store.service, &secrets_bg) {
                            Ok(SecretStorage::Keychain) => {}
                            Ok(SecretStorage::PrivateFile) => {
                                notice = Some(t!("route.secrets_private_file").to_owned());
                            }
                            Ok(SecretStorage::MemoryOnly) => {
                                notice = Some(t!("route.secrets_memory_only").to_owned());
                            }
                            Err(err) => {
                                tracing::warn!(error = %format_args!("{err:#}"), "saving route secrets failed");
                            }
                        }
                    }
                }
                let res = route::apply(cfg_bg, secrets_bg)
                    .map(|snap| snap.config.endpoint_summary())
                    .map_err(|e| e.to_string());
                let _ = tx.send(JobOutcome::Apply {
                    config: Box::new(cfg),
                    result: res,
                    save_error,
                    notice,
                });
                egui.request_repaint();
            });
        self.job = Some(PendingJob { done: rx });
    }

    /// Poll background test/apply work. Returns `true` when a new route was applied.
    pub fn poll(&mut self, ctx: &egui::Context) -> bool {
        let Some(job) = &self.job else {
            return false;
        };
        match job.done.try_recv() {
            Ok(outcome) => {
                self.job = None;
                match outcome {
                    JobOutcome::Test(result) => match result {
                        Ok(msg) => self.test_ok = Some(msg),
                        Err(err) => self.error = Some(err),
                    },
                    JobOutcome::Apply {
                        config,
                        result,
                        save_error,
                        notice,
                    } => {
                        self.synced_generation = route::generation();
                        self.draft = *config;
                        self.socks5_password = secret_input();
                        self.tor_password = secret_input();
                        self.wg_private_key = secret_input();
                        self.wg_preshared_key = secret_input();
                        self.wg_conf = Zeroizing::new(String::new());
                        self.clear_socks5_password = false;
                        self.clear_tor_password = false;
                        self.clear_wg_preshared_key = false;
                        forget(ctx);
                        if let Some(msg) = notice {
                            self.notice = Some(msg);
                        }
                        match result {
                            Ok(_) => {
                                if let Some(err) = save_error {
                                    self.error = Some(err);
                                } else {
                                    self.test_ok = Some(t!("route.applied").to_owned());
                                }
                                return true;
                            }
                            Err(err) => {
                                self.error = Some(err);
                                return true;
                            }
                        }
                    }
                }
            }
            Err(mpsc::TryRecvError::Empty) => {
                ctx.request_repaint_after(POLL);
            }
            Err(mpsc::TryRecvError::Disconnected) => {
                self.job = None;
                self.error = Some(t!("route.worker_stopped").to_owned());
            }
        }
        false
    }

    fn import_wg_conf(&mut self) {
        self.error = None;
        match WireGuardConfig::parse_conf(&self.wg_conf) {
            Ok((wg, priv_key, psk)) => {
                self.wg_addresses = wg.addresses.join(", ");
                self.wg_dns = wg.dns.join(", ");
                self.wg_allowed_ips = wg.allowed_ips.join(", ");
                self.wg_mtu = wg.mtu.to_string();
                self.wg_keepalive = wg
                    .persistent_keepalive
                    .map_or_else(String::new, |s| s.to_string());
                self.draft.wireguard = wg;
                if let Some(key) = priv_key {
                    self.wg_private_key = key;
                }
                if let Some(psk) = psk {
                    self.wg_preshared_key = psk;
                    self.clear_wg_preshared_key = false;
                }
                self.wg_conf = Zeroizing::new(String::new());
                self.notice = Some(t!("route.wg_imported").to_owned());
            }
            Err(err) => {
                self.error = Some(err.to_string());
            }
        }
    }

    fn mode_picker(&mut self, ui: &mut Ui) {
        let changed = ui::segmented(
            ui,
            &mut self.draft.kind,
            &[
                (RouteKind::Direct, t!("route.direct"), ""),
                (RouteKind::Socks5, t!("route.socks5"), ""),
                (RouteKind::WireGuard, t!("route.wireguard"), ""),
                (RouteKind::Tor, t!("route.tor"), ""),
            ],
        );
        if changed {
            self.error = None;
            self.test_ok = None;
            self.notice = None;
        }
    }

    fn mode_fields(&mut self, ui: &mut Ui, compact: bool) {
        let width = if compact {
            ui.available_width()
        } else {
            ui.available_width().min(MAX_FIELD_WIDTH)
        };
        let c = ui::colors(ui);
        match self.draft.kind {
            RouteKind::Direct => {
                ui.label(RichText::new(t!("route.direct_help")).color(c.muted));
            }
            RouteKind::Socks5 => {
                ui.label(RichText::new(t!("route.socks5_help")).color(c.muted));
                ui.add_space(SPACE_S);
                labeled_field(
                    ui,
                    t!("route.host"),
                    "127.0.0.1",
                    &mut self.draft.socks5.host,
                    width,
                );
                labeled_field(ui, t!("route.port"), "1080", &mut self.socks5_port, width);
                labeled_field(
                    ui,
                    t!("route.username_optional"),
                    "",
                    &mut self.draft.socks5.username,
                    width,
                );
                secret_row(
                    ui,
                    t!("route.password_optional"),
                    secret_field_id("socks5-pass"),
                    &mut self.socks5_password,
                    self.draft.socks5.has_password && !self.clear_socks5_password,
                    Some(&mut self.clear_socks5_password),
                    width,
                );
                ui.add_space(SPACE_XS);
                ui.label(ui::caption(t!("route.remote_dns_note")));
            }
            RouteKind::Tor => {
                ui.label(RichText::new(t!("route.tor_help")).color(c.muted));
                ui.add_space(SPACE_S);
                labeled_field(
                    ui,
                    t!("route.host"),
                    "127.0.0.1",
                    &mut self.draft.tor.host,
                    width,
                );
                labeled_field(ui, t!("route.port"), "9050", &mut self.tor_port, width);
                labeled_field(
                    ui,
                    t!("route.tor_isolation_user"),
                    "",
                    &mut self.draft.tor.username,
                    width,
                );
                secret_row(
                    ui,
                    t!("route.tor_isolation_pass"),
                    secret_field_id("tor-pass"),
                    &mut self.tor_password,
                    self.draft.tor.has_password && !self.clear_tor_password,
                    Some(&mut self.clear_tor_password),
                    width,
                );
                ui.add_space(SPACE_XS);
                ui.label(ui::caption(t!("route.tor_ports_hint")));
            }
            RouteKind::WireGuard => {
                ui.label(RichText::new(t!("route.wireguard_help")).color(c.muted));
                ui.add_space(SPACE_S);
                CollapsingHeader::new(t!("route.wg_paste_conf")).show(ui, |ui| {
                    ui.add(
                        ui::text_area(&mut *self.wg_conf)
                            .id(secret_field_id("wg-conf"))
                            .desired_rows(5)
                            .desired_width(width)
                            .private()
                            .hint_text("[Interface]\nPrivateKey = ...\nAddress = 10.0.0.2/32\nDNS = 1.1.1.1\n\n[Peer]\nPublicKey = ...\nEndpoint = host:51820\nAllowedIPs = 0.0.0.0/0, ::/0"),
                    );
                    ui.add_space(SPACE_XS);
                    if ui::secondary(ui, t!("route.wg_import_button")).clicked() {
                        self.import_wg_conf();
                    }
                });
                ui.add_space(SPACE_S);
                labeled_field(
                    ui,
                    t!("route.wg_endpoint"),
                    "vpn.example.com:51820",
                    &mut self.draft.wireguard.endpoint,
                    width,
                );
                labeled_field(
                    ui,
                    t!("route.wg_peer_key"),
                    "",
                    &mut self.draft.wireguard.peer_public_key,
                    width,
                );
                secret_row(
                    ui,
                    t!("route.wg_private_key"),
                    secret_field_id("wg-priv"),
                    &mut self.wg_private_key,
                    self.draft.wireguard.has_private_key,
                    None,
                    width,
                );
                secret_row(
                    ui,
                    t!("route.wg_preshared_key"),
                    secret_field_id("wg-psk"),
                    &mut self.wg_preshared_key,
                    self.draft.wireguard.has_preshared_key && !self.clear_wg_preshared_key,
                    Some(&mut self.clear_wg_preshared_key),
                    width,
                );
                labeled_field(
                    ui,
                    t!("route.wg_addresses"),
                    "10.0.0.2/32, fd00::2/128",
                    &mut self.wg_addresses,
                    width,
                );
                labeled_field(
                    ui,
                    t!("route.wg_dns"),
                    "1.1.1.1, 2606:4700:4700::1111",
                    &mut self.wg_dns,
                    width,
                );
                labeled_field(
                    ui,
                    t!("route.wg_allowed_ips"),
                    "0.0.0.0/0, ::/0",
                    &mut self.wg_allowed_ips,
                    width,
                );
                labeled_field(ui, t!("route.wg_mtu"), "1280", &mut self.wg_mtu, width);
                labeled_field(
                    ui,
                    t!("route.wg_keepalive"),
                    "25",
                    &mut self.wg_keepalive,
                    width,
                );
            }
        }
        if !self.draft.kind.is_direct() {
            ui.add_space(SPACE_S);
            ui.label(ui::caption(t!("route.calls_unavailable_note")));
        }
    }

    fn show_feedback(&self, ui: &mut Ui) {
        let c = ui::colors(ui);
        if self.job.is_some() {
            ui.horizontal(|ui| {
                ui.spinner();
                ui.label(t!("route.connecting"));
            });
        }
        if let Some(ok) = &self.test_ok {
            ui.label(RichText::new(ok).color(c.success));
        }
        if let Some(notice) = &self.notice {
            ui.label(RichText::new(notice).color(c.warning));
        }
        error_text(ui, self.error.as_deref());
    }
}

/// Onboarding step shown BEFORE RustTalk makes its first network request.
/// Returns `true` once the user's chosen route is active and onboarding may proceed.
pub fn onboarding(ui: &mut Ui, editor: &mut RouteEditor) -> bool {
    let applied = editor.poll(ui.ctx());
    if applied && route::is_active() {
        return true;
    }
    ui::step_title(
        ui,
        t!("route.onboarding_title"),
        t!("route.onboarding_text"),
    );
    let busy = editor.job.is_some();
    ui.add_enabled_ui(!busy, |ui| {
        editor.mode_picker(ui);
        ui.add_space(SPACE_M);
        editor.mode_fields(ui, true);
        ui.add_space(SPACE_M);
        editor.show_feedback(ui);
        ui.add_space(SPACE_M);

        if !editor.draft.kind.is_direct() {
            if ui::secondary_large(ui, t!("route.test")).clicked() {
                editor.start_test(ui.ctx());
            }
            ui.add_space(SPACE_S);
        }
        if ui::primary_large(ui, t!("route.continue")).clicked() {
            match editor.build_candidate() {
                Ok((cfg, secrets)) => {
                    editor.start_apply(ui.ctx(), cfg, secrets);
                }
                Err(err) => {
                    editor.error = Some(err);
                }
            }
        }
    });
    false
}

/// Settings -> Network page (desktop and mobile). Returns `true` when the active route
/// changed so `Home` immediately closes existing connections/calls and reconnects.
pub fn settings_page(ui: &mut Ui, editor: &mut RouteEditor) -> bool {
    editor.sync_if_idle();
    let changed = editor.poll(ui.ctx());
    let snap = route::snapshot();
    let c = ui::colors(ui);

    ui::page_section(ui, t!("route.status_section"));
    ui::card(ui, |ui| {
        let (badge, color, detail) = status_with_colors(&snap, c);
        ui.horizontal(|ui| {
            ui.label(ui::bold(badge).color(color));
            if !detail.is_empty() {
                ui.label(RichText::new(detail).color(c.muted));
            }
        });
        if let RouteStatus::Blocked { reason, .. } = &snap.status {
            ui.add_space(SPACE_XS);
            ui.label(RichText::new(reason).color(c.danger_text));
            ui.add_space(SPACE_S);
            ui.horizontal(|ui| {
                if ui::secondary(ui, t!("route.retry")).clicked() && editor.job.is_none() {
                    let secrets = store_ctx()
                        .and_then(|ctx| load_secrets(&ctx.data, &ctx.service).ok())
                        .unwrap_or_default();
                    editor.start_apply(ui.ctx(), snap.config.clone(), secrets);
                }
                if ui::danger(ui, t!("route.switch_direct")).clicked() && editor.job.is_none() {
                    let mut cfg = snap.config.clone();
                    cfg.kind = RouteKind::Direct;
                    let secrets = store_ctx()
                        .and_then(|ctx| load_secrets(&ctx.data, &ctx.service).ok())
                        .unwrap_or_default();
                    editor.draft.kind = RouteKind::Direct;
                    editor.start_apply(ui.ctx(), cfg, secrets);
                }
            });
        }
    });

    ui::page_section(ui, t!("route.connection_section"));
    let busy = editor.job.is_some();
    ui.add_enabled_ui(!busy, |ui| {
        editor.mode_picker(ui);
        ui.add_space(SPACE_M);
        editor.mode_fields(ui, false);
        ui.add_space(SPACE_M);
        editor.show_feedback(ui);
        ui.add_space(SPACE_M);
        ui.horizontal(|ui| {
            if !editor.draft.kind.is_direct() && ui::secondary(ui, t!("route.test")).clicked() {
                editor.start_test(ui.ctx());
            }
            if ui::primary(ui, t!("route.apply")).clicked() {
                match editor.build_candidate() {
                    Ok((cfg, secrets)) => {
                        editor.start_apply(ui.ctx(), cfg, secrets);
                    }
                    Err(err) => {
                        editor.error = Some(err);
                    }
                }
            }
        });
    });

    ui::page_section(ui, t!("route.privacy_section"));
    ui.label(RichText::new(t!("route.privacy_note")).color(c.muted));
    if snap.status.is_active() && snap.config.kind != RouteKind::Direct {
        ui.add_space(SPACE_XS);
        ui.label(ui::caption(t!("route.scope_note")));
    }

    if changed {
        // Ensure UI repaints immediately on route switch.
        ui.ctx().request_repaint();
    }
    changed
}

/// Top banner shown in `Home` whenever the route is `Connecting` or `Blocked`,
/// returning `true` if the user clicked "Network settings".
pub fn status_banner(ui: &mut Ui) -> bool {
    let snap = route::snapshot();
    let (label, fill, text_color) = match &snap.status {
        RouteStatus::Connecting { kind, .. } => {
            let c = ui::colors(ui);
            let a = ui::accent(ui);
            (
                t!("route.banner_connecting", route = kind_label(*kind)),
                c.main.lerp_to_gamma(a.base, 0.16),
                c.strong,
            )
        }
        RouteStatus::Blocked { kind, reason } => {
            let c = ui::colors(ui);
            (
                t!(
                    "route.banner_blocked",
                    route = kind_label(*kind),
                    reason = reason
                ),
                c.main.lerp_to_gamma(c.danger, 0.2),
                c.danger_text,
            )
        }
        RouteStatus::Pending => {
            let c = ui::colors(ui);
            (
                t!("route.banner_pending").to_owned(),
                c.main.lerp_to_gamma(c.warning, 0.2),
                c.strong,
            )
        }
        RouteStatus::Active { .. } => return false,
    };

    let mut open_settings = false;
    let bar = egui::Panel::top("route-status-banner")
        .frame(
            Frame::new()
                .fill(fill)
                .inner_margin(Margin::symmetric(ui::SPACE_M as i8, SPACE_XS as i8)),
        )
        .show(ui, |ui| {
            let row = egui::vec2(ui.available_width(), CONTROL_HEIGHT);
            ui.allocate_ui_with_layout(row, Layout::left_to_right(Align::Center), |ui| {
                ui.add(egui::Label::new(ui::bold(&label).color(text_color)).truncate());
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    open_settings = ui::secondary(ui, t!("route.open_settings")).clicked();
                });
            });
        });
    let rect = bar.response.rect;
    let c = ui::colors(ui);
    ui.painter()
        .hline(rect.x_range(), rect.bottom(), Stroke::new(1.0, c.border));
    open_settings
}

pub fn kind_label(kind: RouteKind) -> &'static str {
    match kind {
        RouteKind::Direct => t!("route.direct"),
        RouteKind::Socks5 => t!("route.socks5"),
        RouteKind::WireGuard => t!("route.wireguard"),
        RouteKind::Tor => t!("route.tor"),
    }
}

fn status_with_colors(snap: &RouteSnapshot, c: &ui::Palette) -> (String, egui::Color32, String) {
    match &snap.status {
        RouteStatus::Pending => (
            t!("route.status_pending").to_owned(),
            c.warning,
            String::new(),
        ),
        RouteStatus::Connecting { kind, detail } => (
            t!("route.status_connecting", route = kind_label(*kind)),
            c.warning,
            detail.clone(),
        ),
        RouteStatus::Active { kind, detail } => (
            t!("route.status_active", route = kind_label(*kind)),
            c.success,
            detail.clone(),
        ),
        RouteStatus::Blocked { kind, .. } => (
            t!("route.status_blocked", route = kind_label(*kind)),
            c.danger_text,
            snap.config.endpoint_summary(),
        ),
    }
}

fn split_csv(text: &str) -> Vec<String> {
    text.split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
        .collect()
}

fn labeled_field(ui: &mut Ui, label: &str, hint: &str, value: &mut String, width: f32) {
    let text = ui.label(label);
    let field = ui
        .add(ui::field(value).hint_text(hint).desired_width(width))
        .labelled_by(text.id);
    field.widget_info(|| WidgetInfo::labeled(Role::TextInput, ui.is_enabled(), label));
    ui.add_space(SPACE_XS);
}

fn secret_row(
    ui: &mut Ui,
    label: &str,
    id: Id,
    value: &mut Zeroizing<String>,
    saved: bool,
    clear: Option<&mut bool>,
    width: f32,
) {
    let text = ui.label(label);
    let hint = if saved {
        t!("route.secret_saved_hint")
    } else {
        ""
    };
    let show_clear = saved && value.is_empty() && clear.is_some();
    let field_width = if show_clear {
        (width - 80.0).max(120.0)
    } else {
        width
    };
    ui.horizontal(|ui| {
        let field = ui
            .add(
                ui::field(&mut **value)
                    .id(id)
                    .password()
                    .hint_text(hint)
                    .desired_width(field_width),
            )
            .labelled_by(text.id);
        field.widget_info(|| WidgetInfo::labeled(Role::PasswordInput, ui.is_enabled(), label));
        if show_clear
            && let Some(clear) = clear
            && ui::secondary(ui, t!("route.clear_secret")).clicked()
        {
            *clear = true;
        }
    });
    ui.add_space(SPACE_XS);
}

/// Acquire the shared mutex for tests that depend on or mutate the process-global
/// network route controller, ensuring `Direct` is active on entry so parallel `cargo test`
/// runs never flake when `route::tests` temporarily enter `Pending` or `Blocked`.
#[cfg(test)]
pub(crate) fn test_lock() -> std::sync::MutexGuard<'static, ()> {
    let guard = tests::ROUTE_TEST_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let snap = route::snapshot();
    if !snap.status.is_active() || snap.config.kind != RouteKind::Direct {
        let _ = route::apply(RouteConfig::default(), RouteSecrets::default());
    }
    guard
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex as StdMutex;

    pub(super) static ROUTE_TEST_LOCK: std::sync::LazyLock<StdMutex<()>> =
        std::sync::LazyLock::new(|| StdMutex::new(()));

    fn reset() {
        *lock(&SESSION_SECRETS) = None;
        let _ = route::apply(RouteConfig::default(), RouteSecrets::default());
    }

    #[test]
    fn cold_start_onboarding_blocks_until_choice_and_existing_profile_stays_ready() {
        let _guard = ROUTE_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let fresh = tempfile::tempdir().unwrap();
        assert_eq!(
            init(fresh.path(), "RustTalk-test-fresh"),
            InitState::Onboarding
        );
        assert_eq!(route::snapshot().status, RouteStatus::Pending);
        assert!(route::http_proxy().is_err());
        assert!(configure_blocking_reqwest(reqwest::blocking::Client::builder()).is_err());
        assert!(route::allow_realtime_media("Voice calls").is_err());
        assert!(route::allow_sip().is_err());

        let existing = tempfile::tempdir().unwrap();
        fs::write(existing.path().join("servers.json"), b"[]").unwrap();
        assert_eq!(
            init(existing.path(), "RustTalk-test-existing"),
            InitState::Ready
        );
        assert!(matches!(
            route::snapshot().status,
            RouteStatus::Active {
                kind: RouteKind::Direct,
                ..
            }
        ));
        reset();
    }

    #[test]
    fn corrupted_route_json_fails_closed_without_direct_fallback() {
        let _guard = ROUTE_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join(FILE), b"{ not valid json").unwrap();
        assert_eq!(init(dir.path(), "RustTalk-test-corrupt"), InitState::Ready);
        assert!(matches!(
            route::snapshot().status,
            RouteStatus::Blocked { .. }
        ));
        assert!(route::http_proxy().is_err());
        assert!(configure_blocking_reqwest(reqwest::blocking::Client::builder()).is_err());
        reset();
    }

    #[test]
    fn route_json_never_contains_secrets_and_renders_dark_and_light() {
        let _guard = ROUTE_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let dir = tempfile::tempdir().unwrap();
        let cfg = RouteConfig {
            kind: RouteKind::WireGuard,
            wireguard: WireGuardConfig {
                addresses: vec!["10.66.0.2/32".into()],
                dns: vec!["10.66.0.1".into()],
                peer_public_key: "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=".into(),
                endpoint: "203.0.113.10:51820".into(),
                has_private_key: true,
                ..Default::default()
            },
            ..Default::default()
        };
        save_config(dir.path(), &cfg).unwrap();
        let raw = fs::read_to_string(dir.path().join(FILE)).unwrap();
        assert!(!raw.contains("private_key\": \""));
        assert!(raw.contains("\"has_private_key\":true"));

        // Render onboarding and Settings -> Network at desktop (1100x700), tablet (820x1180)
        // and phone (390x844) sizes in dark and light themes via egui_kittest.
        use egui_kittest::{Harness, kittest::Queryable};
        let save_dir = std::env::var_os("RUSTTALK_SAVE_ROUTE_SCREENSHOTS").map(PathBuf::from);
        for (size, size_name, save_shot) in [
            (egui::vec2(1100.0, 700.0), "desktop", true),
            (egui::vec2(820.0, 1180.0), "tablet", false),
            (egui::vec2(390.0, 844.0), "phone", true),
        ] {
            for (mode, suffix) in [(ui::Mode::Dark, "dark"), (ui::Mode::Light, "light")] {
                let mut ob_editor = RouteEditor::from_config(cfg.clone(), 1);
                let mut harness = Harness::builder().with_size(size).build_ui(|ui| {
                    ui::install(ui.ctx());
                    ui::apply(
                        ui.ctx(),
                        ui::Appearance {
                            mode,
                            ..Default::default()
                        },
                    );
                    let _ = ui::onboarding(ui, ("network-test", size_name, suffix), None, |ui| {
                        onboarding(ui, &mut ob_editor)
                    });
                });
                harness.step();
                harness.step();
                harness.step();
                assert!(harness.query_by_label(t!("route.continue")).is_some());
                if save_shot && let Some(out) = &save_dir {
                    let img = harness.render().expect("render onboarding");
                    img.save(out.join(format!("route-onboarding-{size_name}-{suffix}.png")))
                        .expect("save onboarding screenshot");
                }

                route::set_blocked(cfg.clone(), "WireGuard handshake timed out");
                let mut settings_editor =
                    RouteEditor::from_config(cfg.clone(), route::generation());
                let mut harness = Harness::builder().with_size(size).build_ui(|ui| {
                    ui::install(ui.ctx());
                    ui::apply(
                        ui.ctx(),
                        ui::Appearance {
                            mode,
                            ..Default::default()
                        },
                    );
                    let c = ui::colors(ui);
                    egui::CentralPanel::default()
                        .frame(Frame::new().fill(c.main).inner_margin(Margin::same(16)))
                        .show(ui, |ui| {
                            egui::ScrollArea::vertical().show(ui, |ui| {
                                let _ = settings_page(ui, &mut settings_editor);
                            });
                        });
                });
                harness.step();
                harness.step();
                assert!(harness.query_by_label(t!("route.apply")).is_some());
                assert!(harness.query_by_label(t!("route.switch_direct")).is_some());
                if save_shot && let Some(out) = &save_dir {
                    let img = harness.render().expect("render settings");
                    img.save(out.join(format!("route-settings-{size_name}-{suffix}.png")))
                        .expect("save settings screenshot");
                }
            }
        }
        reset();
    }

    #[test]
    fn link_cards_and_init_blocking_enforce_socks5h_and_fail_closed() {
        use std::io::{Read, Write};
        use std::net::{TcpListener, TcpStream};
        use std::sync::{
            Arc,
            atomic::{AtomicBool, AtomicUsize, Ordering},
        };
        use std::time::Instant;

        let _guard = ROUTE_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());

        // 1. Controlled HTTP target serving a synthetic HTML page for link cards.
        let http_listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let http_addr = http_listener.local_addr().unwrap();
        let http_hits = Arc::new(AtomicUsize::new(0));
        let http_hits_srv = Arc::clone(&http_hits);
        std::thread::spawn(move || {
            for stream in http_listener.incoming().flatten() {
                http_hits_srv.fetch_add(1, Ordering::SeqCst);
                let mut stream = stream;
                let mut req = [0u8; 1024];
                let _ = stream.read(&mut req);
                let body = "<html><head><title>Routed Card</title></head><body>ok</body></html>";
                let resp = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    body.len(),
                    body
                );
                let _ = stream.write_all(resp.as_bytes());
                let _ = stream.flush();
            }
        });

        // 2. Controlled RFC 1928 SOCKS5h proxy recording domain ATYP=0x03 requests.
        let proxy_listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let proxy_addr = proxy_listener.local_addr().unwrap();
        let observed_hosts: Arc<StdMutex<Vec<String>>> = Arc::new(StdMutex::new(Vec::new()));
        let observed_hosts_srv = Arc::clone(&observed_hosts);
        std::thread::spawn(move || {
            for stream in proxy_listener.incoming().flatten() {
                let hosts = Arc::clone(&observed_hosts_srv);
                std::thread::spawn(move || {
                    let mut client = stream;
                    let mut greet = [0u8; 2];
                    if client.read_exact(&mut greet).is_err() || greet[0] != 0x05 {
                        return;
                    }
                    let mut methods = vec![0u8; greet[1] as usize];
                    if client.read_exact(&mut methods).is_err() {
                        return;
                    }
                    if client.write_all(&[0x05, 0x00]).is_err() {
                        return;
                    }
                    let mut req = [0u8; 4];
                    if client.read_exact(&mut req).is_err() || req[0] != 0x05 || req[1] != 0x01 {
                        return;
                    }
                    if req[3] != 0x03 {
                        // Reject non-domain requests to prove socks5h remote DNS was used.
                        let _ = client.write_all(&[0x05, 0x08, 0x00, 0x01, 0, 0, 0, 0, 0, 0]);
                        return;
                    }
                    let mut len = [0u8; 1];
                    if client.read_exact(&mut len).is_err() {
                        return;
                    }
                    let mut domain = vec![0u8; len[0] as usize];
                    if client.read_exact(&mut domain).is_err() {
                        return;
                    }
                    let mut port = [0u8; 2];
                    if client.read_exact(&mut port).is_err() {
                        return;
                    }
                    let host = String::from_utf8_lossy(&domain).into_owned();
                    hosts.lock().unwrap().push(host);
                    let Ok(mut upstream) = TcpStream::connect(http_addr) else {
                        return;
                    };
                    if client
                        .write_all(&[0x05, 0x00, 0x00, 0x01, 127, 0, 0, 1, 0, 0])
                        .is_err()
                    {
                        return;
                    }
                    let mut c_read = client.try_clone().unwrap();
                    let mut u_write = upstream.try_clone().unwrap();
                    let t = std::thread::spawn(move || {
                        let _ = std::io::copy(&mut c_read, &mut u_write);
                        let _ = u_write.shutdown(std::net::Shutdown::Write);
                    });
                    let _ = std::io::copy(&mut upstream, &mut client);
                    let _ = client.shutdown(std::net::Shutdown::Write);
                    let _ = t.join();
                });
            }
        });

        // 2b. init_blocking refuses Direct on a brand-new profile and fails closed on unreadable secrets.
        let fresh = tempfile::tempdir().unwrap();
        assert!(init_blocking(fresh.path(), "RustTalk-test-fresh-blocking").is_err());
        assert_eq!(route::snapshot().status, RouteStatus::Pending);

        // 3. init_blocking (used by Linux --check-update-now) fails closed when proxy is down
        // or when saved secret storage is unreadable.
        let dead = TcpListener::bind("127.0.0.1:0").unwrap();
        let dead_port = dead.local_addr().unwrap().port();
        drop(dead);
        let dir = tempfile::tempdir().unwrap();
        let mut cfg = RouteConfig {
            kind: RouteKind::Socks5,
            ..Default::default()
        };
        cfg.socks5.host = "127.0.0.1".into();
        cfg.socks5.port = dead_port;
        save_config(dir.path(), &cfg).unwrap();
        fs::write(dir.path().join(FALLBACK_SECRET_FILE), b"{ corrupt json").unwrap();
        assert!(init_blocking(dir.path(), "RustTalk-test-blocking-corrupt-secret").is_err());
        assert!(matches!(
            route::snapshot().status,
            RouteStatus::Blocked {
                kind: RouteKind::Socks5,
                ..
            }
        ));
        let _ = fs::remove_file(dir.path().join(FALLBACK_SECRET_FILE));
        assert!(init_blocking(dir.path(), "RustTalk-test-blocking").is_err());
        assert!(matches!(
            route::snapshot().status,
            RouteStatus::Blocked {
                kind: RouteKind::Socks5,
                ..
            }
        ));
        assert!(route::configure_reqwest(reqwest::Client::builder()).is_err());
        assert!(configure_blocking_reqwest(reqwest::blocking::Client::builder()).is_err());
        assert!(route::allow_sip().is_err());
        let fetcher = crate::link_cards::Fetcher::new();
        let stop = crate::link_cards::Stop {
            cancel: Arc::new(AtomicBool::new(false)),
            deadline: Instant::now() + Duration::from_secs(3),
        };
        let target = url::Url::parse("http://card.route-test.invalid/article").unwrap();
        assert!(fetcher.make(&target, &stop).is_none());
        assert_eq!(http_hits.load(Ordering::SeqCst), 0);

        // 4. Active SOCKS5h blocks literal private IPs & .local without contacting the proxy,
        // and resolves/fetches synthetic .invalid domains strictly via SOCKS5h remote DNS.
        cfg.socks5.port = proxy_addr.port();
        save_config(dir.path(), &cfg).unwrap();
        init_blocking(dir.path(), "RustTalk-test-blocking").unwrap();
        assert!(matches!(
            route::snapshot().status,
            RouteStatus::Active {
                kind: RouteKind::Socks5,
                ..
            }
        ));
        assert!(route::allow_sip().is_err());
        let local_ip = url::Url::parse("http://127.0.0.1/secret").unwrap();
        let mdns = url::Url::parse("http://printer.local/status").unwrap();
        assert!(fetcher.make(&local_ip, &stop).is_none());
        assert!(fetcher.make(&mdns, &stop).is_none());
        assert!(observed_hosts.lock().unwrap().is_empty());

        let (card, picture) = fetcher
            .make(&target, &stop)
            .expect("synthetic .invalid link card routed over SOCKS5h");
        assert_eq!(card.title, "Routed Card");
        assert!(picture.is_none());
        assert_eq!(
            observed_hosts.lock().unwrap().as_slice(),
            &["card.route-test.invalid".to_string()]
        );
        assert_eq!(http_hits.load(Ordering::SeqCst), 1);

        // 5. Applying a route persists route.json inside the worker thread even if the
        // Settings -> Network UI tab is closed and `RouteEditor::poll` is never called.
        let persist_dir = tempfile::tempdir().unwrap();
        set_store_ctx(persist_dir.path(), "RustTalk-test-persist-bg");
        let mut bg_cfg = RouteConfig {
            kind: RouteKind::Socks5,
            ..Default::default()
        };
        bg_cfg.socks5.host = "127.0.0.1".into();
        bg_cfg.socks5.port = proxy_addr.port();
        let mut editor = RouteEditor::default();
        let ctx = egui::Context::default();
        editor.start_apply(&ctx, bg_cfg.clone(), RouteSecrets::default());
        let deadline = Instant::now() + Duration::from_secs(3);
        let saved_path = persist_dir.path().join(FILE);
        while !saved_path.exists() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        let persisted = load_config(&saved_path).expect("route.json saved by background worker");
        assert_eq!(persisted.kind, RouteKind::Socks5);
        assert_eq!(persisted.socks5.port, proxy_addr.port());
        // Wait for apply job to finish before resetting global state.
        while editor.job.is_some() && Instant::now() < deadline {
            let _ = editor.poll(&ctx);
            std::thread::sleep(Duration::from_millis(10));
        }

        reset();
    }
}
