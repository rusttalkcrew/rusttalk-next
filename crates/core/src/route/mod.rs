//! Process-wide per-app network routing: Direct (default), SOCKS5, WireGuard, or Tor.
//!
//! Every client-originated TCP, HTTPS, WebSocket, DNS, LiveKit and SIP path checks this
//! controller before sending traffic. When a non-Direct route is selected, failures enter
//! `RouteStatus::Blocked` and never silently fall back to Direct. Returning to Direct
//! requires an explicit user choice.

pub mod socks5;
pub mod wireguard;

use std::{
    net::{IpAddr, SocketAddr},
    pin::Pin,
    sync::{Arc, LazyLock, Mutex},
    task::{Context, Poll},
    time::Duration,
};

use serde::{Deserialize, Serialize};
use tokio::{
    io::{AsyncRead, AsyncWrite, DuplexStream, ReadBuf},
    net::TcpStream,
    sync::watch,
    time::timeout,
};
use url::{Host, Url};
use zeroize::Zeroizing;

use socks5::{Socks5Auth, connect_socks5, probe_socks5, strip_brackets};
use wireguard::{ParsedWireGuard, WireGuardTunnel, decode_key, parse_endpoint};

const PROBE_TIMEOUT: Duration = Duration::from_secs(10);

/// How RustTalk routes its own outbound network traffic.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RouteKind {
    #[default]
    Direct,
    Socks5,
    WireGuard,
    Tor,
}

impl RouteKind {
    pub fn is_direct(self) -> bool {
        self == Self::Direct
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Direct => "Direct",
            Self::Socks5 => "SOCKS5",
            Self::WireGuard => "WireGuard",
            Self::Tor => "Tor",
        }
    }
}

/// Non-secret SOCKS5 settings persisted in `route.json`.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct Socks5Config {
    #[serde(default)]
    pub host: String,
    #[serde(default = "default_socks5_port")]
    pub port: u16,
    #[serde(default)]
    pub username: String,
    /// True when a password is stored in secure storage and required at cold start.
    #[serde(default)]
    pub has_password: bool,
}

fn default_socks5_port() -> u16 {
    1080
}

impl Default for Socks5Config {
    fn default() -> Self {
        Self {
            host: String::new(),
            port: default_socks5_port(),
            username: String::new(),
            has_password: false,
        }
    }
}

/// Non-secret Tor SOCKS5 settings persisted in `route.json`.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct TorConfig {
    #[serde(default = "default_tor_host")]
    pub host: String,
    #[serde(default = "default_tor_port")]
    pub port: u16,
    /// Optional SOCKS5 username for Tor stream isolation.
    #[serde(default)]
    pub username: String,
    /// True when an optional Tor SOCKS5 password is stored in secure storage.
    #[serde(default)]
    pub has_password: bool,
}

fn default_tor_host() -> String {
    "127.0.0.1".to_owned()
}

fn default_tor_port() -> u16 {
    9050
}

impl Default for TorConfig {
    fn default() -> Self {
        Self {
            host: default_tor_host(),
            port: default_tor_port(),
            username: String::new(),
            has_password: false,
        }
    }
}

/// Non-secret WireGuard settings persisted in `route.json`.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct WireGuardConfig {
    #[serde(default)]
    pub addresses: Vec<String>,
    #[serde(default)]
    pub dns: Vec<String>,
    #[serde(default = "default_wg_mtu")]
    pub mtu: u16,
    #[serde(default)]
    pub peer_public_key: String,
    #[serde(default)]
    pub endpoint: String,
    #[serde(default = "default_wg_allowed_ips")]
    pub allowed_ips: Vec<String>,
    #[serde(default = "default_wg_keepalive")]
    pub persistent_keepalive: Option<u16>,
    /// True when a private key is stored in secure storage.
    #[serde(default)]
    pub has_private_key: bool,
    /// True when an optional preshared key is stored in secure storage.
    #[serde(default)]
    pub has_preshared_key: bool,
}

fn default_wg_mtu() -> u16 {
    1280
}

fn default_wg_allowed_ips() -> Vec<String> {
    vec!["0.0.0.0/0".to_owned(), "::/0".to_owned()]
}

fn default_wg_keepalive() -> Option<u16> {
    Some(25)
}

impl Default for WireGuardConfig {
    fn default() -> Self {
        Self {
            addresses: Vec::new(),
            dns: Vec::new(),
            mtu: default_wg_mtu(),
            peer_public_key: String::new(),
            endpoint: String::new(),
            allowed_ips: default_wg_allowed_ips(),
            persistent_keepalive: default_wg_keepalive(),
            has_private_key: false,
            has_preshared_key: false,
        }
    }
}

pub type ParsedWireGuardConf = (
    WireGuardConfig,
    Option<Zeroizing<String>>,
    Option<Zeroizing<String>>,
);

impl WireGuardConfig {
    /// Parse a standard WireGuard `.conf` (`[Interface]` / `[Peer]`) text into non-secret
    /// config and zeroized secrets.
    pub fn parse_conf(text: &str) -> Result<ParsedWireGuardConf, RouteError> {
        let mut cfg = Self {
            allowed_ips: Vec::new(),
            ..Self::default()
        };
        let mut private_key: Option<Zeroizing<String>> = None;
        let mut preshared_key: Option<Zeroizing<String>> = None;
        let mut section = "";

        for raw_line in text.lines() {
            let line = raw_line.split('#').next().unwrap_or("").trim();
            if line.is_empty() {
                continue;
            }
            if line.starts_with('[') && line.ends_with(']') {
                section = line[1..line.len() - 1].trim();
                continue;
            }
            let Some((k, v)) = line.split_once('=') else {
                continue;
            };
            let key = k.trim().to_ascii_lowercase();
            let val = v.trim();
            if section.eq_ignore_ascii_case("interface") {
                match key.as_str() {
                    "privatekey" => {
                        decode_key(val, "private key")?;
                        private_key = Some(Zeroizing::new(val.to_owned()));
                        cfg.has_private_key = true;
                    }
                    "address" => {
                        for part in val.split(',') {
                            let part = part.trim();
                            if !part.is_empty() {
                                cfg.addresses.push(part.to_owned());
                            }
                        }
                    }
                    "dns" => {
                        for part in val.split(',') {
                            let part = part.trim();
                            if !part.is_empty() {
                                cfg.dns.push(part.to_owned());
                            }
                        }
                    }
                    "mtu" => {
                        let mtu: u16 = val
                            .parse()
                            .map_err(|_| RouteError::Config("invalid WireGuard MTU".into()))?;
                        cfg.mtu = mtu.clamp(576, 1500);
                    }
                    _ => {}
                }
            } else if section.eq_ignore_ascii_case("peer") {
                match key.as_str() {
                    "publickey" => {
                        decode_key(val, "peer public key")?;
                        cfg.peer_public_key = val.to_owned();
                    }
                    "presharedkey" => {
                        decode_key(val, "preshared key")?;
                        preshared_key = Some(Zeroizing::new(val.to_owned()));
                        cfg.has_preshared_key = true;
                    }
                    "endpoint" => {
                        parse_endpoint(val)?;
                        cfg.endpoint = val.to_owned();
                    }
                    "allowedips" => {
                        for part in val.split(',') {
                            let part = part.trim();
                            if !part.is_empty() {
                                cfg.allowed_ips.push(part.to_owned());
                            }
                        }
                    }
                    "persistentkeepalive" => {
                        let secs: u16 = val.parse().map_err(|_| {
                            RouteError::Config("invalid WireGuard PersistentKeepalive".into())
                        })?;
                        cfg.persistent_keepalive = (secs > 0).then_some(secs);
                    }
                    _ => {}
                }
            }
        }
        if cfg.allowed_ips.is_empty() {
            cfg.allowed_ips = default_wg_allowed_ips();
        }
        // Validate the complete parsed config (with extracted private key).
        ParsedWireGuard::from_config(
            &cfg,
            private_key.as_deref().map(String::as_str),
            preshared_key.as_deref().map(String::as_str),
        )?;
        Ok((cfg, private_key, preshared_key))
    }
}

/// Complete non-secret routing configuration stored per device in `route.json`.
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct RouteConfig {
    #[serde(default)]
    pub kind: RouteKind,
    #[serde(default)]
    pub socks5: Socks5Config,
    #[serde(default)]
    pub tor: TorConfig,
    #[serde(default)]
    pub wireguard: WireGuardConfig,
}

impl RouteConfig {
    pub fn endpoint_summary(&self) -> String {
        match self.kind {
            RouteKind::Direct => String::new(),
            RouteKind::Socks5 => format_host_port(&self.socks5.host, self.socks5.port),
            RouteKind::Tor => format_host_port(&self.tor.host, self.tor.port),
            RouteKind::WireGuard => self.wireguard.endpoint.trim().to_owned(),
        }
    }
}

/// Route secrets stored exclusively in device secure storage (never in `route.json` or logs).
#[derive(Clone, Default, Serialize, Deserialize)]
pub struct RouteSecrets {
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        with = "opt_zeroizing_string"
    )]
    pub socks5_password: Option<Zeroizing<String>>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        with = "opt_zeroizing_string"
    )]
    pub tor_password: Option<Zeroizing<String>>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        with = "opt_zeroizing_string"
    )]
    pub wg_private_key: Option<Zeroizing<String>>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        with = "opt_zeroizing_string"
    )]
    pub wg_preshared_key: Option<Zeroizing<String>>,
}

mod opt_zeroizing_string {
    use serde::{Deserialize, Deserializer, Serializer};
    use zeroize::Zeroizing;

    pub fn serialize<S: Serializer>(
        value: &Option<Zeroizing<String>>,
        serializer: S,
    ) -> Result<S::Ok, S::Error> {
        match value {
            Some(s) => serializer.serialize_some(s.as_str()),
            None => serializer.serialize_none(),
        }
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(
        deserializer: D,
    ) -> Result<Option<Zeroizing<String>>, D::Error> {
        Ok(Option::<String>::deserialize(deserializer)?.map(Zeroizing::new))
    }
}

impl RouteSecrets {
    pub fn is_empty(&self) -> bool {
        self.socks5_password.as_deref().is_none_or(|s| s.is_empty())
            && self.tor_password.as_deref().is_none_or(|s| s.is_empty())
            && self.wg_private_key.as_deref().is_none_or(|s| s.is_empty())
            && self
                .wg_preshared_key
                .as_deref()
                .is_none_or(|s| s.is_empty())
    }
}

impl std::fmt::Debug for RouteSecrets {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RouteSecrets")
            .field(
                "socks5_password",
                &self.socks5_password.as_ref().map(|_| "[REDACTED]"),
            )
            .field(
                "tor_password",
                &self.tor_password.as_ref().map(|_| "[REDACTED]"),
            )
            .field(
                "wg_private_key",
                &self.wg_private_key.as_ref().map(|_| "[REDACTED]"),
            )
            .field(
                "wg_preshared_key",
                &self.wg_preshared_key.as_ref().map(|_| "[REDACTED]"),
            )
            .finish()
    }
}

/// Live state of the app's network route.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum RouteStatus {
    /// Onboarding has not chosen a route yet; all outbound traffic is blocked.
    Pending,
    /// Establishing or verifying the selected route; outbound app traffic is blocked.
    Connecting { kind: RouteKind, detail: String },
    /// Route is active and carrying RustTalk traffic.
    Active { kind: RouteKind, detail: String },
    /// Route failed or is missing credentials; all outbound app traffic is blocked.
    Blocked { kind: RouteKind, reason: String },
}

impl RouteStatus {
    pub fn is_active(&self) -> bool {
        matches!(self, Self::Active { .. })
    }

    pub fn kind(&self) -> Option<RouteKind> {
        match self {
            Self::Pending => None,
            Self::Connecting { kind, .. }
            | Self::Active { kind, .. }
            | Self::Blocked { kind, .. } => Some(*kind),
        }
    }
}

/// Immutable snapshot of the current route state and monotonic generation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RouteSnapshot {
    pub generation: u64,
    pub config: RouteConfig,
    pub status: RouteStatus,
}

/// Errors returned when a route blocks traffic, is misconfigured, or does not support a feature.
#[derive(Debug, Clone, thiserror::Error)]
pub enum RouteError {
    #[error("network route is blocked: {0}")]
    Blocked(String),
    #[error("{feature} is unavailable while routing through {}: {reason}", kind.label())]
    Unsupported {
        feature: &'static str,
        kind: RouteKind,
        reason: String,
    },
    #[error("invalid network route configuration: {0}")]
    Config(String),
    #[error("network route transport failed: {0}")]
    Transport(String),
}

/// A TCP byte stream established through the active route.
pub enum RouteStream {
    Direct(TcpStream),
    WireGuard(DuplexStream),
}

impl AsyncRead for RouteStream {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        match self.get_mut() {
            Self::Direct(s) => Pin::new(s).poll_read(cx, buf),
            Self::WireGuard(s) => Pin::new(s).poll_read(cx, buf),
        }
    }
}

impl AsyncWrite for RouteStream {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        match self.get_mut() {
            Self::Direct(s) => Pin::new(s).poll_write(cx, buf),
            Self::WireGuard(s) => Pin::new(s).poll_write(cx, buf),
        }
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        match self.get_mut() {
            Self::Direct(s) => Pin::new(s).poll_flush(cx),
            Self::WireGuard(s) => Pin::new(s).poll_flush(cx),
        }
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        match self.get_mut() {
            Self::Direct(s) => Pin::new(s).poll_shutdown(cx),
            Self::WireGuard(s) => Pin::new(s).poll_shutdown(cx),
        }
    }
}

/// How sender-side link previews should resolve a URL without leaking local DNS.
#[derive(Debug)]
pub enum LinkCardResolution {
    /// Direct mode: use local OS DNS + public IP check.
    Direct,
    /// SOCKS5 / Tor: pass hostname remotely through `socks5h://` (literal private IPs already refused).
    RemoteDns,
    /// WireGuard: already resolved inside the tunnel and checked against `public(ip)`.
    Resolved(Vec<SocketAddr>),
}

enum ActiveBackend {
    Pending,
    Direct,
    Socks5 {
        host: String,
        port: u16,
        auth: Option<Socks5Auth>,
        proxy: reqwest::Proxy,
    },
    Tor {
        host: String,
        port: u16,
        auth: Option<Socks5Auth>,
        proxy: reqwest::Proxy,
    },
    WireGuard(Arc<WireGuardTunnel>),
    Blocked(String),
}

struct ControllerInner {
    generation: u64,
    config: RouteConfig,
    status: RouteStatus,
    backend: ActiveBackend,
}

struct Controller {
    inner: Mutex<ControllerInner>,
    watch: watch::Sender<RouteSnapshot>,
}

static CONTROLLER: LazyLock<Controller> = LazyLock::new(|| {
    let config = RouteConfig::default();
    let status = RouteStatus::Active {
        kind: RouteKind::Direct,
        detail: String::new(),
    };
    let snapshot = RouteSnapshot {
        generation: 1,
        config: config.clone(),
        status: status.clone(),
    };
    let (watch, _) = watch::channel(snapshot);
    Controller {
        inner: Mutex::new(ControllerInner {
            generation: 1,
            config,
            status,
            backend: ActiveBackend::Direct,
        }),
        watch,
    }
});

impl Controller {
    fn publish(&self, inner: &ControllerInner) {
        let _ = self.watch.send(RouteSnapshot {
            generation: inner.generation,
            config: inner.config.clone(),
            status: inner.status.clone(),
        });
    }
}

/// Current route snapshot.
pub fn snapshot() -> RouteSnapshot {
    let guard = CONTROLLER.inner.lock().expect("route lock");
    RouteSnapshot {
        generation: guard.generation,
        config: guard.config.clone(),
        status: guard.status.clone(),
    }
}

/// Subscribe to route status and generation changes.
pub fn subscribe() -> watch::Receiver<RouteSnapshot> {
    CONTROLLER.watch.subscribe()
}

pub fn generation() -> u64 {
    CONTROLLER.inner.lock().expect("route lock").generation
}

pub fn is_active() -> bool {
    CONTROLLER
        .inner
        .lock()
        .expect("route lock")
        .status
        .is_active()
}

/// Block all outbound traffic until the user picks a route during onboarding.
pub fn enter_pending() -> RouteSnapshot {
    let (snap, old_backend) = {
        let mut guard = CONTROLLER.inner.lock().expect("route lock");
        guard.generation += 1;
        guard.config = RouteConfig::default();
        guard.status = RouteStatus::Pending;
        let old = std::mem::replace(&mut guard.backend, ActiveBackend::Pending);
        CONTROLLER.publish(&guard);
        (
            RouteSnapshot {
                generation: guard.generation,
                config: guard.config.clone(),
                status: guard.status.clone(),
            },
            old,
        )
    };
    drop(old_backend);
    snap
}

/// Mark the selected route as connecting (blocks all traffic until activation completes).
pub fn set_connecting(config: RouteConfig) -> u64 {
    let (generation, old_backend) = {
        let mut guard = CONTROLLER.inner.lock().expect("route lock");
        guard.generation += 1;
        let detail = config.endpoint_summary();
        guard.status = RouteStatus::Connecting {
            kind: config.kind,
            detail,
        };
        guard.config = config;
        let old = std::mem::replace(
            &mut guard.backend,
            ActiveBackend::Blocked("connecting to selected network route".into()),
        );
        let generation = guard.generation;
        CONTROLLER.publish(&guard);
        (generation, old)
    };
    drop(old_backend);
    generation
}

/// Mark the selected route as blocked with a clear reason (never falling back to Direct).
pub fn set_blocked(config: RouteConfig, reason: impl Into<String>) -> RouteSnapshot {
    let reason = reason.into();
    let (snap, old_backend) = {
        let mut guard = CONTROLLER.inner.lock().expect("route lock");
        guard.generation += 1;
        guard.status = RouteStatus::Blocked {
            kind: config.kind,
            reason: reason.clone(),
        };
        guard.config = config;
        let old = std::mem::replace(&mut guard.backend, ActiveBackend::Blocked(reason));
        CONTROLLER.publish(&guard);
        (
            RouteSnapshot {
                generation: guard.generation,
                config: guard.config.clone(),
                status: guard.status.clone(),
            },
            old,
        )
    };
    drop(old_backend);
    snap
}

/// Mark the current active route as blocked if `expected_generation` is still current.
/// Used when an active tunnel expires or a proxy stops responding mid-session.
pub fn fail_active(expected_generation: u64, reason: impl Into<String>) {
    let reason = reason.into();
    let old_backend = {
        let mut guard = CONTROLLER.inner.lock().expect("route lock");
        if guard.generation != expected_generation || guard.config.kind.is_direct() {
            return;
        }
        guard.generation += 1;
        guard.status = RouteStatus::Blocked {
            kind: guard.config.kind,
            reason: reason.clone(),
        };
        let old = std::mem::replace(&mut guard.backend, ActiveBackend::Blocked(reason));
        CONTROLLER.publish(&guard);
        old
    };
    drop(old_backend);
}

/// Validate a route configuration and secrets without opening any network connection.
pub fn validate(config: &RouteConfig, secrets: &RouteSecrets) -> Result<(), RouteError> {
    match config.kind {
        RouteKind::Direct => Ok(()),
        RouteKind::Socks5 => {
            validate_socks_endpoint(&config.socks5.host, config.socks5.port, "SOCKS5")?;
            let _ = build_socks_auth(
                &config.socks5.username,
                config.socks5.has_password,
                secrets.socks5_password.as_ref(),
                "SOCKS5",
            )?;
            Ok(())
        }
        RouteKind::Tor => {
            validate_socks_endpoint(&config.tor.host, config.tor.port, "Tor SOCKS5")?;
            let _ = build_socks_auth(
                &config.tor.username,
                config.tor.has_password,
                secrets.tor_password.as_ref(),
                "Tor SOCKS5",
            )?;
            Ok(())
        }
        RouteKind::WireGuard => {
            let _ = ParsedWireGuard::from_config(
                &config.wireguard,
                secrets.wg_private_key.as_deref().map(String::as_str),
                secrets.wg_preshared_key.as_deref().map(String::as_str),
            )?;
            Ok(())
        }
    }
}

/// Test a candidate route without disturbing the currently active route.
/// Returns a short status detail on success.
pub fn test_route(config: &RouteConfig, secrets: &RouteSecrets) -> Result<String, RouteError> {
    validate(config, secrets)?;
    match config.kind {
        RouteKind::Direct => Ok("Direct connection ready".into()),
        RouteKind::Socks5 => {
            let auth = build_socks_auth(
                &config.socks5.username,
                config.socks5.has_password,
                secrets.socks5_password.as_ref(),
                "SOCKS5",
            )?;
            let host = config.socks5.host.trim().to_owned();
            let port = config.socks5.port;
            run_one_shot(
                async move { probe_socks5(&host, port, auth.as_ref(), PROBE_TIMEOUT).await },
            )?;
            Ok(format!(
                "SOCKS5 proxy reachable at {}",
                format_host_port(&config.socks5.host, config.socks5.port)
            ))
        }
        RouteKind::Tor => {
            let auth = build_socks_auth(
                &config.tor.username,
                config.tor.has_password,
                secrets.tor_password.as_ref(),
                "Tor SOCKS5",
            )?;
            let host = config.tor.host.trim().to_owned();
            let port = config.tor.port;
            run_one_shot(
                async move { probe_socks5(&host, port, auth.as_ref(), PROBE_TIMEOUT).await },
            )?;
            Ok(format!(
                "Tor SOCKS5 proxy reachable at {}",
                format_host_port(&config.tor.host, config.tor.port)
            ))
        }
        RouteKind::WireGuard => {
            let parsed = ParsedWireGuard::from_config(
                &config.wireguard,
                secrets.wg_private_key.as_deref().map(String::as_str),
                secrets.wg_preshared_key.as_deref().map(String::as_str),
            )?;
            let endpoint = format_host_port(&parsed.endpoint_host, parsed.endpoint_port);
            let tunnel = WireGuardTunnel::start(parsed, Arc::new(|_| {}))?;
            drop(tunnel);
            Ok(format!("WireGuard handshake succeeded with {endpoint}"))
        }
    }
}

/// Apply a route configuration and secrets, tearing down any previous tunnel and
/// verifying connectivity before activating. On failure, enters `RouteStatus::Blocked`
/// and NEVER silently falls back to Direct.
pub fn apply(config: RouteConfig, secrets: RouteSecrets) -> Result<RouteSnapshot, RouteError> {
    let kind = config.kind;
    let detail = config.endpoint_summary();

    if kind == RouteKind::Direct {
        let (snap, old_backend) = {
            let mut guard = CONTROLLER.inner.lock().expect("route lock");
            guard.generation += 1;
            guard.config = config;
            guard.status = RouteStatus::Active {
                kind: RouteKind::Direct,
                detail: String::new(),
            };
            let old = std::mem::replace(&mut guard.backend, ActiveBackend::Direct);
            CONTROLLER.publish(&guard);
            (
                RouteSnapshot {
                    generation: guard.generation,
                    config: guard.config.clone(),
                    status: guard.status.clone(),
                },
                old,
            )
        };
        drop(old_backend);
        return Ok(snap);
    }

    // Enter Connecting (blocks all app traffic while establishing the new route).
    let connecting_gen = set_connecting(config.clone());

    let built: Result<ActiveBackend, RouteError> = match kind {
        RouteKind::Direct => unreachable!(),
        RouteKind::Socks5 => (|| {
            validate_socks_endpoint(&config.socks5.host, config.socks5.port, "SOCKS5")?;
            let auth = build_socks_auth(
                &config.socks5.username,
                config.socks5.has_password,
                secrets.socks5_password.as_ref(),
                "SOCKS5",
            )?;
            let host = config.socks5.host.trim().to_owned();
            let port = config.socks5.port;
            let probe_host = host.clone();
            let probe_auth = auth.clone();
            run_one_shot(async move {
                probe_socks5(&probe_host, port, probe_auth.as_ref(), PROBE_TIMEOUT).await
            })?;
            let proxy = build_socks5h_proxy(&host, port, auth.as_ref())?;
            Ok(ActiveBackend::Socks5 {
                host,
                port,
                auth,
                proxy,
            })
        })(),
        RouteKind::Tor => (|| {
            validate_socks_endpoint(&config.tor.host, config.tor.port, "Tor SOCKS5")?;
            let auth = build_socks_auth(
                &config.tor.username,
                config.tor.has_password,
                secrets.tor_password.as_ref(),
                "Tor SOCKS5",
            )?;
            let host = config.tor.host.trim().to_owned();
            let port = config.tor.port;
            let probe_host = host.clone();
            let probe_auth = auth.clone();
            run_one_shot(async move {
                probe_socks5(&probe_host, port, probe_auth.as_ref(), PROBE_TIMEOUT).await
            })?;
            let proxy = build_socks5h_proxy(&host, port, auth.as_ref())?;
            Ok(ActiveBackend::Tor {
                host,
                port,
                auth,
                proxy,
            })
        })(),
        RouteKind::WireGuard => (|| {
            let parsed = ParsedWireGuard::from_config(
                &config.wireguard,
                secrets.wg_private_key.as_deref().map(String::as_str),
                secrets.wg_preshared_key.as_deref().map(String::as_str),
            )?;
            // Target generation when committed will be `connecting_gen + 1`.
            let active_gen = connecting_gen + 1;
            let tunnel = WireGuardTunnel::start(
                parsed,
                Arc::new(move |reason| fail_active(active_gen, reason)),
            )?;
            Ok(ActiveBackend::WireGuard(Arc::new(tunnel)))
        })(),
    };

    let (result, to_drop) = {
        let mut guard = CONTROLLER.inner.lock().expect("route lock");
        // Another apply superseded this one while connecting.
        if guard.generation != connecting_gen {
            (
                Err(RouteError::Blocked(
                    "superseded by a newer route change".into(),
                )),
                built.ok(),
            )
        } else {
            // Close the startup-vs-expiry race: do not publish Active if the tunnel already died.
            let built = match built {
                Ok(ActiveBackend::WireGuard(tunnel)) if !tunnel.is_alive() => Err((
                    RouteError::Blocked(
                        "WireGuard tunnel expired before activation completed".into(),
                    ),
                    Some(ActiveBackend::WireGuard(tunnel)),
                )),
                Ok(backend) => Ok(backend),
                Err(err) => Err((err, None)),
            };
            match built {
                Ok(backend) => {
                    guard.generation += 1;
                    guard.config = config;
                    guard.status = RouteStatus::Active { kind, detail };
                    let old = std::mem::replace(&mut guard.backend, backend);
                    CONTROLLER.publish(&guard);
                    tracing::info!(route = kind.label(), "network route active");
                    (
                        Ok(RouteSnapshot {
                            generation: guard.generation,
                            config: guard.config.clone(),
                            status: guard.status.clone(),
                        }),
                        Some(old),
                    )
                }
                Err((err, dead_backend)) => {
                    let reason = err.to_string();
                    guard.generation += 1;
                    guard.config = config;
                    guard.status = RouteStatus::Blocked {
                        kind,
                        reason: reason.clone(),
                    };
                    let old = std::mem::replace(&mut guard.backend, ActiveBackend::Blocked(reason));
                    CONTROLLER.publish(&guard);
                    tracing::warn!(route = kind.label(), %err, "network route blocked");
                    // Drop `dead_backend` outside the lock alongside `old` by returning it first;
                    // `old` is always `Blocked("connecting...")` here, never a live tunnel.
                    drop(old);
                    (Err(err), dead_backend)
                }
            }
        }
    };
    drop(to_drop);
    result
}

/// Return the `reqwest::Proxy` for the active route (`None` for Direct), or an error
/// if the route is pending, connecting, or blocked.
pub fn http_proxy() -> Result<Option<reqwest::Proxy>, RouteError> {
    let guard = CONTROLLER.inner.lock().expect("route lock");
    match &guard.backend {
        ActiveBackend::Direct => Ok(None),
        ActiveBackend::Socks5 { proxy, .. } | ActiveBackend::Tor { proxy, .. } => {
            Ok(Some(proxy.clone()))
        }
        ActiveBackend::WireGuard(tunnel) => Ok(Some(tunnel.reqwest_proxy()?)),
        ActiveBackend::Pending => Err(RouteError::Blocked(
            "choose a network route before connecting".into(),
        )),
        ActiveBackend::Blocked(reason) => Err(RouteError::Blocked(reason.clone())),
    }
}

/// Return a `reqwest::Proxy` for requests whose destination IPs were already resolved and
/// validated (e.g. sender-side link cards). For WireGuard this returns the `socks5://`
/// bridge so `reqwest` uses our strict pinned DNS resolver and sends the pre-validated IP
/// literal to the tunnel bridge instead of triggering a second in-tunnel DNS lookup.
pub fn http_proxy_for_resolved() -> Result<Option<reqwest::Proxy>, RouteError> {
    let guard = CONTROLLER.inner.lock().expect("route lock");
    match &guard.backend {
        ActiveBackend::Direct => Ok(None),
        ActiveBackend::Socks5 { proxy, .. } | ActiveBackend::Tor { proxy, .. } => {
            Ok(Some(proxy.clone()))
        }
        ActiveBackend::WireGuard(tunnel) => Ok(Some(tunnel.reqwest_proxy_resolved()?)),
        ActiveBackend::Pending => Err(RouteError::Blocked(
            "choose a network route before connecting".into(),
        )),
        ActiveBackend::Blocked(reason) => Err(RouteError::Blocked(reason.clone())),
    }
}

/// Strict `reqwest::dns::Resolve` override that answers ONLY for one pre-validated host
/// with its pre-validated addresses and refuses all other DNS lookups (zero OS DNS and
/// zero second in-tunnel DNS lookup / DNS rebinding).
#[derive(Clone, Debug)]
pub struct PinnedDnsResolver {
    host: String,
    addrs: Vec<SocketAddr>,
}

impl PinnedDnsResolver {
    pub fn new(host: impl Into<String>, addrs: Vec<SocketAddr>) -> Self {
        Self {
            host: host.into(),
            addrs,
        }
    }
}

impl reqwest::dns::Resolve for PinnedDnsResolver {
    fn resolve(&self, name: reqwest::dns::Name) -> reqwest::dns::Resolving {
        let host = self.host.clone();
        let addrs = self.addrs.clone();
        Box::pin(async move {
            if name.as_str().eq_ignore_ascii_case(&host) {
                let iter: reqwest::dns::Addrs = Box::new(addrs.into_iter());
                Ok(iter)
            } else {
                Err(Box::new(std::io::Error::new(
                    std::io::ErrorKind::PermissionDenied,
                    "refusing unexpected DNS lookup for pinned destination",
                ))
                    as Box<dyn std::error::Error + Send + Sync>)
            }
        })
    }
}

/// Apply the active route to an async `reqwest::ClientBuilder`.
/// Always disables OS/environment proxy auto-discovery (`.no_proxy()`), then installs
/// the active route's `socks5h://` proxy when routed.
pub fn configure_reqwest(
    builder: reqwest::ClientBuilder,
) -> Result<reqwest::ClientBuilder, RouteError> {
    let builder = builder.no_proxy();
    match http_proxy()? {
        None => Ok(builder),
        Some(proxy) => Ok(builder.proxy(proxy)),
    }
}

/// Open a TCP stream to `(host, port)` through the active route (never leaking local DNS
/// when SOCKS5, Tor, or WireGuard is active).
pub async fn connect_tcp(
    host: &str,
    port: u16,
    limit: Duration,
) -> Result<RouteStream, RouteError> {
    enum Plan {
        Direct,
        Socks5(String, u16, Option<Socks5Auth>),
        WireGuard(Arc<WireGuardTunnel>),
    }
    let plan = {
        let guard = CONTROLLER.inner.lock().expect("route lock");
        match &guard.backend {
            ActiveBackend::Direct => Plan::Direct,
            ActiveBackend::Socks5 {
                host, port, auth, ..
            }
            | ActiveBackend::Tor {
                host, port, auth, ..
            } => Plan::Socks5(host.clone(), *port, auth.clone()),
            ActiveBackend::WireGuard(tunnel) => Plan::WireGuard(tunnel.clone()),
            ActiveBackend::Pending => {
                return Err(RouteError::Blocked(
                    "choose a network route before connecting".into(),
                ));
            }
            ActiveBackend::Blocked(reason) => {
                return Err(RouteError::Blocked(reason.clone()));
            }
        }
    };
    match plan {
        Plan::Direct => {
            let clean = strip_brackets(host);
            let connect = async {
                if let Ok(ip) = clean.parse::<IpAddr>() {
                    TcpStream::connect(SocketAddr::new(ip, port)).await
                } else {
                    TcpStream::connect((clean, port)).await
                }
            };
            let stream = timeout(limit, connect)
                .await
                .map_err(|_| RouteError::Transport("TCP connection timed out".into()))?
                .map_err(|e| RouteError::Transport(format!("TCP connection failed: {e}")))?;
            let _ = stream.set_nodelay(true);
            Ok(RouteStream::Direct(stream))
        }
        Plan::Socks5(proxy_host, proxy_port, auth) => {
            let stream =
                connect_socks5(&proxy_host, proxy_port, auth.as_ref(), host, port, limit).await?;
            Ok(RouteStream::Direct(stream))
        }
        Plan::WireGuard(tunnel) => {
            let stream = tunnel.connect_tcp(host, port, limit).await?;
            Ok(RouteStream::WireGuard(stream))
        }
    }
}

/// Gate for LiveKit WebRTC media and data channels (voice, video, screen share,
/// shared terminal, remote control, and live file transfers).
///
/// The upstream `livekit` / `libwebrtc` C++ stack opens direct OS UDP/TCP sockets and
/// gathers host ICE candidates with no proxy/socket-factory hook. Allowing it under
/// app-only SOCKS5, WireGuard, or Tor would silently bypass the route and leak local
/// addresses. We fail closed with a clear explanation instead.
pub fn allow_realtime_media(feature: &'static str) -> Result<(), RouteError> {
    let guard = CONTROLLER.inner.lock().expect("route lock");
    match &guard.status {
        RouteStatus::Active {
            kind: RouteKind::Direct,
            ..
        } => Ok(()),
        RouteStatus::Pending => Err(RouteError::Blocked(
            "choose a network route before connecting".into(),
        )),
        RouteStatus::Connecting { .. } => Err(RouteError::Blocked(
            "waiting for the selected network route to connect".into(),
        )),
        RouteStatus::Blocked { reason, .. } => Err(RouteError::Blocked(reason.clone())),
        RouteStatus::Active { kind, .. } => Err(RouteError::Unsupported {
            feature,
            kind: *kind,
            reason: match kind {
                RouteKind::Tor => {
                    "Tor supports TCP only and cannot carry WebRTC media without leaking direct UDP/ICE traffic".into()
                }
                RouteKind::Socks5 => {
                    "WebRTC media cannot route through SOCKS5 without leaking direct UDP/ICE addresses".into()
                }
                RouteKind::WireGuard => {
                    "App-only WireGuard routes HTTPS and WebSockets in userspace; WebRTC media requires Direct mode or a system WireGuard VPN to avoid leaking direct UDP/ICE traffic".into()
                }
                RouteKind::Direct => unreachable!(),
            },
        }),
    }
}

/// Gate for desktop SIP phone calls (UDP signaling + RTP media).
pub fn allow_sip() -> Result<(), RouteError> {
    let guard = CONTROLLER.inner.lock().expect("route lock");
    match &guard.status {
        RouteStatus::Active {
            kind: RouteKind::Direct,
            ..
        } => Ok(()),
        RouteStatus::Pending => Err(RouteError::Blocked(
            "choose a network route before connecting".into(),
        )),
        RouteStatus::Connecting { .. } => Err(RouteError::Blocked(
            "waiting for the selected network route to connect".into(),
        )),
        RouteStatus::Blocked { reason, .. } => Err(RouteError::Blocked(reason.clone())),
        RouteStatus::Active { kind, .. } => Err(RouteError::Unsupported {
            feature: "SIP phone calls",
            kind: *kind,
            reason: "SIP signaling and RTP media use direct UDP sockets and are disabled while app-only routing is active to prevent direct DNS/UDP leaks".into(),
        }),
    }
}

/// Determine how a sender-side link preview should resolve `url` without leaking local DNS.
/// `is_public` checks whether an IP address is on the public internet (SSRF guard).
pub fn resolve_for_link_card(
    url: &Url,
    allow_local: bool,
    is_public: fn(IpAddr) -> bool,
    limit: Duration,
) -> Result<LinkCardResolution, RouteError> {
    let port = url
        .port_or_known_default()
        .ok_or_else(|| RouteError::Config("URL has no port".into()))?;
    let host = url
        .host()
        .ok_or_else(|| RouteError::Config("URL has no host".into()))?;

    enum Mode {
        Direct,
        Proxy,
        WireGuard(Arc<WireGuardTunnel>),
    }
    let mode = {
        let guard = CONTROLLER.inner.lock().expect("route lock");
        match &guard.backend {
            ActiveBackend::Direct => Mode::Direct,
            ActiveBackend::Socks5 { .. } | ActiveBackend::Tor { .. } => Mode::Proxy,
            ActiveBackend::WireGuard(t) => Mode::WireGuard(t.clone()),
            ActiveBackend::Pending => {
                return Err(RouteError::Blocked(
                    "choose a network route before connecting".into(),
                ));
            }
            ActiveBackend::Blocked(reason) => return Err(RouteError::Blocked(reason.clone())),
        }
    };

    match mode {
        Mode::Direct => Ok(LinkCardResolution::Direct),
        Mode::Proxy => {
            // Never call local OS DNS. Still block literal private IPs and local-only hostnames.
            match host {
                Host::Ipv4(ip) if !allow_local && !is_public(IpAddr::V4(ip)) => {
                    return Err(RouteError::Blocked(
                        "refusing non-public IP address for link preview".into(),
                    ));
                }
                Host::Ipv6(ip) if !allow_local && !is_public(IpAddr::V6(ip)) => {
                    return Err(RouteError::Blocked(
                        "refusing non-public IP address for link preview".into(),
                    ));
                }
                Host::Domain(domain) if !allow_local => {
                    let lower = domain.trim_end_matches('.').to_ascii_lowercase();
                    if lower == "localhost"
                        || lower.ends_with(".localhost")
                        || lower.ends_with(".local")
                        || lower.ends_with(".internal")
                        || !lower.contains('.')
                    {
                        return Err(RouteError::Blocked(
                            "refusing local hostname for link preview".into(),
                        ));
                    }
                }
                _ => {}
            }
            Ok(LinkCardResolution::RemoteDns)
        }
        Mode::WireGuard(tunnel) => {
            let ips = match host {
                Host::Ipv4(ip) => vec![IpAddr::V4(ip)],
                Host::Ipv6(ip) => vec![IpAddr::V6(ip)],
                Host::Domain(domain) => {
                    let domain = domain.to_owned();
                    run_one_shot(async move { tunnel.resolve_dns(&domain, limit).await })?
                }
            };
            if ips.is_empty() || (!allow_local && !ips.iter().all(|&ip| is_public(ip))) {
                return Err(RouteError::Blocked(
                    "link preview destination resolved to a non-public address".into(),
                ));
            }
            Ok(LinkCardResolution::Resolved(
                ips.into_iter()
                    .map(|ip| SocketAddr::new(ip, port))
                    .collect(),
            ))
        }
    }
}

fn validate_socks_endpoint(host: &str, port: u16, label: &str) -> Result<(), RouteError> {
    let clean = strip_brackets(host.trim());
    if clean.is_empty() || clean.contains(char::is_whitespace) {
        return Err(RouteError::Config(format!("{label} host is required")));
    }
    if port == 0 {
        return Err(RouteError::Config(format!(
            "{label} port must be between 1 and 65535"
        )));
    }
    Ok(())
}

fn build_socks_auth(
    username: &str,
    has_password: bool,
    password: Option<&Zeroizing<String>>,
    label: &str,
) -> Result<Option<Socks5Auth>, RouteError> {
    let user = username.trim();
    let pass = password.map(|p| p.as_str()).filter(|p| !p.is_empty());
    if user.is_empty() && pass.is_none() {
        if has_password {
            return Err(RouteError::Blocked(format!(
                "{label} password is missing from secure storage. Re-enter it in Settings or switch to Direct."
            )));
        }
        return Ok(None);
    }
    if has_password && pass.is_none() {
        return Err(RouteError::Blocked(format!(
            "{label} password is missing from secure storage. Re-enter it in Settings or switch to Direct."
        )));
    }
    Ok(Some(Socks5Auth {
        username: user.to_owned(),
        password: Zeroizing::new(pass.unwrap_or("").to_owned()),
    }))
}

fn build_socks5h_proxy(
    host: &str,
    port: u16,
    auth: Option<&Socks5Auth>,
) -> Result<reqwest::Proxy, RouteError> {
    let url = format!("socks5h://{}", format_host_port(host, port));
    let mut proxy = reqwest::Proxy::all(&url)
        .map_err(|e| RouteError::Config(format!("invalid SOCKS5 proxy address: {e}")))?;
    if let Some(auth) = auth {
        proxy = proxy.basic_auth(&auth.username, auth.password.as_str());
    }
    Ok(proxy)
}

pub fn format_host_port(host: &str, port: u16) -> String {
    let clean = strip_brackets(host.trim());
    if clean.contains(':') {
        format!("[{clean}]:{port}")
    } else {
        format!("{clean}:{port}")
    }
}

/// Run an async helper safely whether called from a sync thread or inside an existing
/// Tokio runtime (by running on a short-lived helper thread if already inside Tokio).
fn run_one_shot<T, F>(future: F) -> Result<T, RouteError>
where
    T: Send + 'static,
    F: std::future::Future<Output = Result<T, RouteError>> + Send + 'static,
{
    if tokio::runtime::Handle::try_current().is_ok() {
        std::thread::spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .map_err(|e| RouteError::Transport(format!("start route runtime: {e}")))?;
            rt.block_on(future)
        })
        .join()
        .map_err(|_| RouteError::Transport("route helper thread panicked".into()))?
    } else {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|e| RouteError::Transport(format!("start route runtime: {e}")))?;
        rt.block_on(future)
    }
}

#[cfg(test)]
mod tests;
