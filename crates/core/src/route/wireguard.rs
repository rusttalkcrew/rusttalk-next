//! App-only userspace WireGuard tunnel (`boringtun` + `smoltcp`).
//!
//! Runs completely in userspace over a single unprivileged UDP socket to the peer
//! endpoint: no TUN/TAP interface, no root privileges, no OS VPN entitlements, and
//! zero changes to host-wide routing or other apps' traffic. Works identically on
//! Linux, macOS, Windows, Android, and iOS.

use std::{
    collections::{HashMap, VecDeque},
    net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant as StdInstant},
};

use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64};
use boringtun::{
    noise::{Tunn, TunnResult, errors::WireGuardError},
    x25519::{PublicKey, StaticSecret},
};
use smoltcp::{
    iface::{Config as IfaceConfig, Interface, SocketHandle, SocketSet},
    phy::{Device, DeviceCapabilities, Medium, RxToken, TxToken},
    socket::{
        dns::{self, GetQueryResultError, QueryHandle},
        tcp,
    },
    time::Instant as SmolInstant,
    wire::{DnsQueryType, HardwareAddress, IpAddress, IpCidr, IpEndpoint},
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt, DuplexStream},
    net::UdpSocket,
    sync::{mpsc, oneshot},
    time::{Instant, sleep},
};
use zeroize::Zeroizing;

use super::{RouteError, WireGuardConfig, socks5::LoopbackSocks5};

const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);
const DNS_TIMEOUT: Duration = Duration::from_secs(8);
const TIMER_INTERVAL: Duration = Duration::from_millis(250);
const TCP_BUF_SIZE: usize = 65_536;
const DUPLEX_BUF_SIZE: usize = 65_536;
const MAX_WG_PACKET: usize = 2048;

/// Validated, parsed WireGuard parameters ready to launch a userspace tunnel.
#[derive(Clone)]
pub struct ParsedWireGuard {
    pub addresses: Vec<IpCidr>,
    pub dns: Vec<IpAddr>,
    pub mtu: u16,
    pub peer_public_key: [u8; 32],
    pub endpoint_host: String,
    pub endpoint_port: u16,
    pub allowed_ips: Vec<IpCidr>,
    pub persistent_keepalive: Option<u16>,
    pub private_key: Zeroizing<[u8; 32]>,
    pub preshared_key: Option<Zeroizing<[u8; 32]>>,
}

impl std::fmt::Debug for ParsedWireGuard {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ParsedWireGuard")
            .field("addresses", &self.addresses)
            .field("dns", &self.dns)
            .field("mtu", &self.mtu)
            .field("endpoint_host", &self.endpoint_host)
            .field("endpoint_port", &self.endpoint_port)
            .field("allowed_ips", &self.allowed_ips)
            .field("persistent_keepalive", &self.persistent_keepalive)
            .field("private_key", &"[REDACTED]")
            .field(
                "preshared_key",
                &self.preshared_key.as_ref().map(|_| "[REDACTED]"),
            )
            .finish()
    }
}

impl ParsedWireGuard {
    pub fn from_config(
        cfg: &WireGuardConfig,
        private_key: Option<&str>,
        preshared_key: Option<&str>,
    ) -> Result<Self, RouteError> {
        let addresses = parse_cidrs(&cfg.addresses, "tunnel address")?;
        if addresses.is_empty() {
            return Err(RouteError::Config(
                "WireGuard requires at least one tunnel IPv4 or IPv6 address (e.g. 10.0.0.2/32)"
                    .into(),
            ));
        }
        let dns = parse_ips(&cfg.dns)?;
        let allowed_ips = if cfg.allowed_ips.is_empty() {
            vec![
                IpCidr::new(IpAddress::v4(0, 0, 0, 0), 0),
                IpCidr::new(IpAddress::v6(0, 0, 0, 0, 0, 0, 0, 0), 0),
            ]
        } else {
            parse_cidrs(&cfg.allowed_ips, "AllowedIPs")?
        };
        let mtu = cfg.mtu.clamp(576, 1500);
        let peer_public_key = decode_key(&cfg.peer_public_key, "peer public key")?;
        let raw_private = private_key
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .ok_or_else(|| {
                RouteError::Blocked(
                    "WireGuard private key is missing from secure storage. Re-enter it in Settings or switch to Direct."
                        .into(),
                )
            })?;
        let private_key = Zeroizing::new(decode_key(raw_private, "private key")?);
        let preshared_key = match preshared_key.map(str::trim).filter(|s| !s.is_empty()) {
            Some(psk) => Some(Zeroizing::new(decode_key(psk, "preshared key")?)),
            None if cfg.has_preshared_key => {
                return Err(RouteError::Blocked(
                    "WireGuard preshared key is missing from secure storage. Re-enter it in Settings or switch to Direct."
                        .into(),
                ));
            }
            None => None,
        };
        let (endpoint_host, endpoint_port) = parse_endpoint(&cfg.endpoint)?;
        Ok(Self {
            addresses,
            dns,
            mtu,
            peer_public_key,
            endpoint_host,
            endpoint_port,
            allowed_ips,
            persistent_keepalive: cfg.persistent_keepalive.filter(|&secs| secs > 0),
            private_key,
            preshared_key,
        })
    }

    pub fn allows(&self, ip: IpAddr) -> bool {
        let addr = to_smol_ip(ip);
        self.allowed_ips
            .iter()
            .any(|cidr| cidr.contains_addr(&addr))
    }

    fn has_ipv4(&self) -> bool {
        self.addresses
            .iter()
            .any(|cidr| matches!(cidr, IpCidr::Ipv4(_)))
    }

    fn has_ipv6(&self) -> bool {
        self.addresses
            .iter()
            .any(|cidr| matches!(cidr, IpCidr::Ipv6(_)))
    }
}

pub(crate) fn decode_key(raw: &str, label: &str) -> Result<[u8; 32], RouteError> {
    let decoded =
        Zeroizing::new(BASE64.decode(raw.trim()).map_err(|_| {
            RouteError::Config(format!("invalid WireGuard {label} (expected base64)"))
        })?);
    if decoded.len() != 32 {
        return Err(RouteError::Config(format!(
            "WireGuard {label} must decode to 32 bytes"
        )));
    }
    let mut key = [0u8; 32];
    key.copy_from_slice(&decoded);
    Ok(key)
}

pub(crate) fn parse_endpoint(raw: &str) -> Result<(String, u16), RouteError> {
    let text = raw.trim();
    if text.is_empty() {
        return Err(RouteError::Config(
            "WireGuard endpoint is required (host:port)".into(),
        ));
    }
    if let Ok(addr) = text.parse::<SocketAddr>() {
        if addr.port() == 0 {
            return Err(RouteError::Config(
                "WireGuard endpoint port cannot be 0".into(),
            ));
        }
        return Ok((addr.ip().to_string(), addr.port()));
    }
    let (host, port_str) = if let Some(rest) = text.strip_prefix('[') {
        let (h, p) = rest
            .split_once("]:")
            .ok_or_else(|| RouteError::Config("invalid IPv6 WireGuard endpoint".into()))?;
        (h, p)
    } else {
        text.rsplit_once(':')
            .ok_or_else(|| RouteError::Config("WireGuard endpoint must be host:port".into()))?
    };
    let port: u16 = port_str
        .parse()
        .ok()
        .filter(|&p| p > 0)
        .ok_or_else(|| RouteError::Config("invalid WireGuard endpoint port".into()))?;
    if host.is_empty() || host.contains(char::is_whitespace) {
        return Err(RouteError::Config("invalid WireGuard endpoint host".into()));
    }
    Ok((host.to_owned(), port))
}

fn parse_cidrs(items: &[String], label: &str) -> Result<Vec<IpCidr>, RouteError> {
    let mut out = Vec::new();
    for raw in items {
        for part in raw.split(',') {
            let text = part.trim();
            if text.is_empty() {
                continue;
            }
            let (ip_str, prefix_str) = match text.split_once('/') {
                Some((ip, p)) => (ip.trim(), Some(p.trim())),
                None => (text, None),
            };
            let ip: IpAddr = ip_str
                .parse()
                .map_err(|_| RouteError::Config(format!("invalid WireGuard {label}: {text}")))?;
            let max_prefix = match ip {
                IpAddr::V4(_) => 32,
                IpAddr::V6(_) => 128,
            };
            let prefix: u8 = match prefix_str {
                Some(p) => p.parse().ok().filter(|&n| n <= max_prefix).ok_or_else(|| {
                    RouteError::Config(format!("invalid prefix in {label}: {text}"))
                })?,
                None => max_prefix,
            };
            out.push(IpCidr::new(to_smol_ip(ip), prefix));
        }
    }
    Ok(out)
}

fn parse_ips(items: &[String]) -> Result<Vec<IpAddr>, RouteError> {
    let mut out = Vec::new();
    for raw in items {
        for part in raw.split(',') {
            let text = part.trim();
            if text.is_empty() {
                continue;
            }
            let ip: IpAddr = text
                .parse()
                .map_err(|_| RouteError::Config(format!("invalid WireGuard DNS IP: {text}")))?;
            out.push(ip);
        }
    }
    Ok(out)
}

fn to_smol_ip(ip: IpAddr) -> IpAddress {
    match ip {
        IpAddr::V4(v4) => IpAddress::Ipv4(v4),
        IpAddr::V6(v6) => IpAddress::Ipv6(v6),
    }
}

fn from_smol_ip(ip: IpAddress) -> IpAddr {
    match ip {
        IpAddress::Ipv4(v4) => IpAddr::V4(v4),
        IpAddress::Ipv6(v6) => IpAddr::V6(v6),
    }
}

/// A live, running userspace WireGuard tunnel backed by `boringtun` + `smoltcp`.
/// Dropping it shuts down the tunnel, closes all streams, and stops the loopback bridge.
pub(crate) struct WireGuardTunnel {
    commands: mpsc::UnboundedSender<ReactorCmd>,
    proxy: reqwest::Proxy,
    resolved_proxy: reqwest::Proxy,
    alive: Arc<AtomicBool>,
    shutdown: Option<oneshot::Sender<()>>,
    worker: Option<std::thread::JoinHandle<()>>,
}

impl WireGuardTunnel {
    /// Start the userspace WireGuard tunnel and wait for the initial Noise handshake
    /// to complete. Fails closed if the peer does not complete the handshake.
    pub(crate) fn start(
        parsed: ParsedWireGuard,
        on_failure: Arc<dyn Fn(String) + Send + Sync + 'static>,
    ) -> Result<Self, RouteError> {
        let (ready_tx, ready_rx) = std::sync::mpsc::sync_channel(1);
        let (shutdown_tx, shutdown_rx) = oneshot::channel();
        let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();
        let alive = Arc::new(AtomicBool::new(false));
        let alive_worker = alive.clone();
        let cmd_for_bridge = cmd_tx.clone();

        let worker = std::thread::Builder::new()
            .name("rusttalk-wg".into())
            .spawn(move || {
                let rt = match tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                {
                    Ok(rt) => rt,
                    Err(e) => {
                        let _ = ready_tx.send(Err(RouteError::Transport(format!(
                            "start WireGuard runtime: {e}"
                        ))));
                        return;
                    }
                };
                rt.block_on(run_tunnel(
                    parsed,
                    cmd_rx,
                    cmd_for_bridge,
                    alive_worker,
                    ready_tx,
                    shutdown_rx,
                    on_failure,
                ));
            })
            .map_err(|e| RouteError::Transport(format!("spawn WireGuard thread: {e}")))?;

        let (proxy, resolved_proxy) = match ready_rx.recv() {
            Ok(Ok(proxies)) => proxies,
            Ok(Err(err)) => {
                let _ = shutdown_tx.send(());
                let _ = worker.join();
                return Err(err);
            }
            Err(_) => {
                let _ = worker.join();
                return Err(RouteError::Blocked(
                    "WireGuard tunnel stopped before handshake completed".into(),
                ));
            }
        };

        Ok(Self {
            commands: cmd_tx,
            proxy,
            resolved_proxy,
            alive,
            shutdown: Some(shutdown_tx),
            worker: Some(worker),
        })
    }

    pub(crate) fn is_alive(&self) -> bool {
        self.alive.load(Ordering::Acquire)
    }

    pub(crate) fn reqwest_proxy(&self) -> Result<reqwest::Proxy, RouteError> {
        if !self.is_alive() {
            return Err(RouteError::Blocked(
                "WireGuard tunnel is no longer active".into(),
            ));
        }
        Ok(self.proxy.clone())
    }

    pub(crate) fn reqwest_proxy_resolved(&self) -> Result<reqwest::Proxy, RouteError> {
        if !self.is_alive() {
            return Err(RouteError::Blocked(
                "WireGuard tunnel is no longer active".into(),
            ));
        }
        Ok(self.resolved_proxy.clone())
    }

    pub(crate) async fn connect_tcp(
        &self,
        host: &str,
        port: u16,
        limit: Duration,
    ) -> Result<DuplexStream, RouteError> {
        if !self.alive.load(Ordering::Acquire) {
            return Err(RouteError::Blocked(
                "WireGuard tunnel is no longer active".into(),
            ));
        }
        connect_via_channel(&self.commands, host.to_owned(), port, limit).await
    }

    pub(crate) async fn resolve_dns(
        &self,
        domain: &str,
        limit: Duration,
    ) -> Result<Vec<IpAddr>, RouteError> {
        if !self.alive.load(Ordering::Acquire) {
            return Err(RouteError::Blocked(
                "WireGuard tunnel is no longer active".into(),
            ));
        }
        let (reply_tx, reply_rx) = oneshot::channel();
        self.commands
            .send(ReactorCmd::Resolve {
                domain: domain.to_owned(),
                reply: reply_tx,
            })
            .map_err(|_| RouteError::Blocked("WireGuard tunnel stopped".into()))?;
        tokio::time::timeout(limit, reply_rx)
            .await
            .map_err(|_| RouteError::Transport("WireGuard DNS lookup timed out".into()))?
            .map_err(|_| RouteError::Blocked("WireGuard tunnel stopped".into()))?
    }
}

impl Drop for WireGuardTunnel {
    fn drop(&mut self) {
        self.alive.store(false, Ordering::Release);
        if let Some(tx) = self.shutdown.take() {
            let _ = tx.send(());
        }
        if let Some(handle) = self.worker.take() {
            // Avoid blocking on self if ever dropped from the worker thread.
            if std::thread::current().id() != handle.thread().id() {
                let _ = handle.join();
            }
        }
    }
}

async fn connect_via_channel(
    commands: &mpsc::UnboundedSender<ReactorCmd>,
    host: String,
    port: u16,
    limit: Duration,
) -> Result<DuplexStream, RouteError> {
    let (reply_tx, reply_rx) = oneshot::channel();
    commands
        .send(ReactorCmd::Connect {
            host,
            port,
            reply: reply_tx,
        })
        .map_err(|_| RouteError::Blocked("WireGuard tunnel stopped".into()))?;
    tokio::time::timeout(limit, reply_rx)
        .await
        .map_err(|_| RouteError::Transport("WireGuard TCP connection timed out".into()))?
        .map_err(|_| RouteError::Blocked("WireGuard tunnel stopped".into()))?
}

enum ReactorCmd {
    Connect {
        host: String,
        port: u16,
        reply: oneshot::Sender<Result<DuplexStream, RouteError>>,
    },
    Resolve {
        domain: String,
        reply: oneshot::Sender<Result<Vec<IpAddr>, RouteError>>,
    },
    Wake,
}

async fn run_tunnel(
    parsed: ParsedWireGuard,
    mut cmd_rx: mpsc::UnboundedReceiver<ReactorCmd>,
    cmd_tx: mpsc::UnboundedSender<ReactorCmd>,
    alive: Arc<AtomicBool>,
    ready_tx: std::sync::mpsc::SyncSender<Result<(reqwest::Proxy, reqwest::Proxy), RouteError>>,
    mut shutdown_rx: oneshot::Receiver<()>,
    on_failure: Arc<dyn Fn(String) + Send + Sync + 'static>,
) {
    let endpoint_addr = match resolve_endpoint(&parsed.endpoint_host, parsed.endpoint_port).await {
        Ok(addr) => addr,
        Err(err) => {
            let _ = ready_tx.send(Err(err));
            return;
        }
    };
    let bind_addr: SocketAddr = if endpoint_addr.is_ipv4() {
        SocketAddr::from((Ipv4Addr::UNSPECIFIED, 0))
    } else {
        SocketAddr::from((Ipv6Addr::UNSPECIFIED, 0))
    };
    let udp = match UdpSocket::bind(bind_addr).await {
        Ok(sock) => sock,
        Err(e) => {
            let _ = ready_tx.send(Err(RouteError::Blocked(format!(
                "opening WireGuard UDP socket failed: {e}"
            ))));
            return;
        }
    };
    if let Err(e) = udp.connect(endpoint_addr).await {
        let _ = ready_tx.send(Err(RouteError::Blocked(format!(
            "connecting WireGuard UDP socket failed: {e}"
        ))));
        return;
    }

    let static_secret = StaticSecret::from(*parsed.private_key);
    let peer_public = PublicKey::from(parsed.peer_public_key);
    let psk = parsed.preshared_key.as_deref().copied();
    let mut index_bytes = [0u8; 4];
    let _ = crate::identity::random_bytes(&mut index_bytes);
    let index = u32::from_le_bytes(index_bytes) & 0x00FF_FFFF;
    let mut tunn = Tunn::new(
        static_secret,
        peer_public,
        psk,
        parsed.persistent_keepalive,
        index,
        None,
    );

    // Initiate the Noise_IKpsk2 handshake and wait for completion before opening traffic.
    let mut wg_out = vec![0u8; MAX_WG_PACKET];
    let mut wg_in = vec![0u8; MAX_WG_PACKET];
    if let TunnResult::WriteToNetwork(packet) = tunn.format_handshake_initiation(&mut wg_out, false)
        && let Err(e) = udp.send(packet).await
    {
        let _ = ready_tx.send(Err(RouteError::Blocked(format!(
            "sending WireGuard handshake failed: {e}"
        ))));
        return;
    }

    let deadline = Instant::now() + HANDSHAKE_TIMEOUT;
    let mut timer = tokio::time::interval(TIMER_INTERVAL);
    let mut handshake_ok = false;
    while Instant::now() < deadline {
        tokio::select! {
            _ = &mut shutdown_rx => {
                let _ = ready_tx.send(Err(RouteError::Blocked("WireGuard tunnel cancelled".into())));
                return;
            }
            _ = timer.tick() => {
                match tunn.update_timers(&mut wg_out) {
                    TunnResult::WriteToNetwork(packet) => {
                        let _ = udp.send(packet).await;
                    }
                    TunnResult::Err(WireGuardError::ConnectionExpired) => {
                        let _ = ready_tx.send(Err(RouteError::Blocked(
                            "WireGuard handshake expired (peer unreachable or key mismatch)".into(),
                        )));
                        return;
                    }
                    _ => {}
                }
            }
            recv = udp.recv(&mut wg_in) => {
                let Ok(n) = recv else {
                    let _ = ready_tx.send(Err(RouteError::Blocked(
                        "WireGuard UDP socket returned an error during handshake".into(),
                    )));
                    return;
                };
                match tunn.decapsulate(Some(endpoint_addr.ip()), &wg_in[..n], &mut wg_out) {
                    TunnResult::WriteToNetwork(packet) => {
                        let _ = udp.send(packet).await;
                        // Drain queued packets / handshake confirm if any.
                        while let TunnResult::WriteToNetwork(more) = tunn.decapsulate(None, &[], &mut wg_out) {
                            let _ = udp.send(more).await;
                        }
                    }
                    TunnResult::Err(err) => {
                        tracing::warn!(?err, "WireGuard packet rejected during handshake");
                    }
                    _ => {}
                }
                if tunn.stats().0.is_some() {
                    handshake_ok = true;
                    break;
                }
            }
        }
    }

    if !handshake_ok {
        let _ = ready_tx.send(Err(RouteError::Blocked(
            "WireGuard handshake timed out (check endpoint, firewall and keys)".into(),
        )));
        return;
    }

    // Build smoltcp IPv4/IPv6 + TCP + DNS interface.
    let mut device = WgDevice::new(usize::from(parsed.mtu));
    let mut seed_bytes = [0u8; 8];
    let _ = crate::identity::random_bytes(&mut seed_bytes);
    let mut iface_cfg = IfaceConfig::new(HardwareAddress::Ip);
    iface_cfg.random_seed = u64::from_le_bytes(seed_bytes);
    let start_instant = StdInstant::now();
    let smol_now = |start: StdInstant| SmolInstant::from_millis(start.elapsed().as_millis() as i64);
    let mut iface = Interface::new(iface_cfg, &mut device, smol_now(start_instant));
    iface.update_ip_addrs(|addrs| {
        for cidr in &parsed.addresses {
            let _ = addrs.push(*cidr);
        }
    });
    if parsed.has_ipv4() {
        let _ = iface
            .routes_mut()
            .add_default_ipv4_route(Ipv4Addr::UNSPECIFIED);
    }
    if parsed.has_ipv6() {
        let _ = iface
            .routes_mut()
            .add_default_ipv6_route(Ipv6Addr::UNSPECIFIED);
    }

    let mut sockets = SocketSet::new(vec![]);
    let dns_servers: Vec<IpAddress> = parsed
        .dns
        .iter()
        .copied()
        .filter(|ip| (ip.is_ipv4() && parsed.has_ipv4()) || (ip.is_ipv6() && parsed.has_ipv6()))
        .map(to_smol_ip)
        .collect();
    let dns_handle = if !dns_servers.is_empty() {
        let queries: Vec<Option<dns::DnsQuery>> = (0..16).map(|_| None).collect();
        let sock = dns::Socket::new(&dns_servers, queries);
        Some(sockets.add(sock))
    } else {
        None
    };

    // Start the authenticated localhost SOCKS5h bridge into this WireGuard reactor.
    let bridge_cmds = cmd_tx.clone();
    let bridge = match LoopbackSocks5::start(move |host, port| {
        let commands = bridge_cmds.clone();
        async move { connect_via_channel(&commands, host, port, CONNECT_TIMEOUT).await }
    })
    .await
    {
        Ok(bridge) => bridge,
        Err(err) => {
            let _ = ready_tx.send(Err(err));
            return;
        }
    };
    let proxy = match bridge.reqwest_proxy() {
        Ok(proxy) => proxy,
        Err(err) => {
            let _ = ready_tx.send(Err(err));
            return;
        }
    };
    let resolved_proxy = match bridge.reqwest_proxy_resolved() {
        Ok(proxy) => proxy,
        Err(err) => {
            let _ = ready_tx.send(Err(err));
            return;
        }
    };

    alive.store(true, Ordering::Release);
    let _ = ready_tx.send(Ok((proxy, resolved_proxy)));

    let mut reactor = ReactorState::new(parsed, cmd_tx, dns_handle);

    loop {
        let now = smol_now(start_instant);
        iface.poll(now, &mut device, &mut sockets);
        reactor.step(&mut iface, &mut sockets);
        iface.poll(smol_now(start_instant), &mut device, &mut sockets);

        // Encrypt & transmit every raw IP packet smoltcp produced.
        while let Some(ip_packet) = device.tx_queue.pop_front() {
            match tunn.encapsulate(&ip_packet, &mut wg_out) {
                TunnResult::WriteToNetwork(encrypted) => {
                    if let Err(e) = udp.send(encrypted).await {
                        tracing::warn!(%e, "WireGuard UDP send failed");
                    }
                }
                TunnResult::Err(err) => {
                    tracing::warn!(?err, "WireGuard encapsulate failed");
                }
                _ => {}
            }
        }

        let poll_wait = iface
            .poll_delay(smol_now(start_instant), &sockets)
            .map(|d| Duration::from_millis(d.total_millis().max(1)))
            .unwrap_or(Duration::from_millis(50))
            .min(Duration::from_millis(50));

        tokio::select! {
            _ = &mut shutdown_rx => {
                break;
            }
            _ = sleep(poll_wait) => {}
            _ = timer.tick() => {
                match tunn.update_timers(&mut wg_out) {
                    TunnResult::WriteToNetwork(packet) => {
                        let _ = udp.send(packet).await;
                    }
                    TunnResult::Err(WireGuardError::ConnectionExpired) => {
                        alive.store(false, Ordering::Release);
                        on_failure("WireGuard tunnel connection expired".into());
                        break;
                    }
                    _ => {}
                }
            }
            cmd = cmd_rx.recv() => {
                match cmd {
                    Some(cmd) => reactor.handle_cmd(cmd, &mut iface, &mut sockets),
                    None => break,
                }
            }
            recv = udp.recv(&mut wg_in) => {
                match recv {
                    Ok(n) => {
                        let mut res = tunn.decapsulate(Some(endpoint_addr.ip()), &wg_in[..n], &mut wg_out);
                        loop {
                            match res {
                                TunnResult::WriteToNetwork(packet) => {
                                    let _ = udp.send(packet).await;
                                    res = tunn.decapsulate(None, &[], &mut wg_out);
                                }
                                TunnResult::WriteToTunnelV4(ip, _) => {
                                    if ip.len() >= 20 && (ip[0] >> 4) == 4 {
                                        let src = IpAddr::V4(Ipv4Addr::new(
                                            ip[12], ip[13], ip[14], ip[15],
                                        ));
                                        if reactor.parsed.allows(src) {
                                            device.rx_queue.push_back(ip.to_vec());
                                        } else {
                                            tracing::warn!(%src, "dropping WireGuard IPv4 packet outside AllowedIPs");
                                        }
                                    }
                                    break;
                                }
                                TunnResult::WriteToTunnelV6(ip, _) => {
                                    if ip.len() >= 40 && (ip[0] >> 4) == 6 {
                                        let mut bytes = [0u8; 16];
                                        bytes.copy_from_slice(&ip[8..24]);
                                        let src = IpAddr::V6(Ipv6Addr::from(bytes));
                                        if reactor.parsed.allows(src) {
                                            device.rx_queue.push_back(ip.to_vec());
                                        } else {
                                            tracing::warn!(%src, "dropping WireGuard IPv6 packet outside AllowedIPs");
                                        }
                                    }
                                    break;
                                }
                                TunnResult::Done => break,
                                TunnResult::Err(err) => {
                                    tracing::warn!(?err, "WireGuard decapsulate error");
                                    break;
                                }
                            }
                        }
                    }
                    Err(e) => {
                        alive.store(false, Ordering::Release);
                        on_failure(format!("WireGuard UDP transport error: {e}"));
                        break;
                    }
                }
            }
        }
    }

    alive.store(false, Ordering::Release);
    drop(bridge);
}

async fn resolve_endpoint(host: &str, port: u16) -> Result<SocketAddr, RouteError> {
    if let Ok(ip) = host.parse::<IpAddr>() {
        return Ok(SocketAddr::new(ip, port));
    }
    tokio::time::timeout(DNS_TIMEOUT, tokio::net::lookup_host((host, port)))
        .await
        .map_err(|_| RouteError::Blocked("resolving WireGuard endpoint timed out".into()))?
        .map_err(|e| RouteError::Blocked(format!("resolving WireGuard endpoint failed: {e}")))?
        .next()
        .ok_or_else(|| RouteError::Blocked("WireGuard endpoint resolved to no addresses".into()))
}

struct ReactorState {
    parsed: ParsedWireGuard,
    wake_tx: mpsc::UnboundedSender<ReactorCmd>,
    dns_handle: Option<SocketHandle>,
    next_port: u16,
    pending_dns: Vec<PendingDns>,
    tcp_conns: HashMap<SocketHandle, TcpBridge>,
}

struct PendingDns {
    queries: Vec<QueryHandle>,
    results: Vec<IpAddr>,
    deadline: StdInstant,
    action: DnsAction,
}

enum DnsAction {
    Resolve(oneshot::Sender<Result<Vec<IpAddr>, RouteError>>),
    Connect {
        port: u16,
        reply: oneshot::Sender<Result<DuplexStream, RouteError>>,
    },
}

struct TcpBridge {
    /// None once handshake succeeded and caller received their DuplexStream half.
    connect_reply: Option<oneshot::Sender<Result<DuplexStream, RouteError>>>,
    /// Bytes waiting to be written into the smoltcp TCP socket.
    to_net: VecDeque<u8>,
    /// Channel receiving writes from the caller's DuplexStream pump.
    from_app_rx: mpsc::Receiver<Vec<u8>>,
    /// Channel sending received bytes from smoltcp into the caller's DuplexStream pump.
    to_app_tx: mpsc::Sender<Vec<u8>>,
    app_closed: bool,
    net_eof_sent: bool,
    started: StdInstant,
}

impl ReactorState {
    fn new(
        parsed: ParsedWireGuard,
        wake_tx: mpsc::UnboundedSender<ReactorCmd>,
        dns_handle: Option<SocketHandle>,
    ) -> Self {
        Self {
            parsed,
            wake_tx,
            dns_handle,
            next_port: 49152,
            pending_dns: Vec::new(),
            tcp_conns: HashMap::new(),
        }
    }

    fn alloc_port(&mut self) -> u16 {
        let port = self.next_port;
        self.next_port = if self.next_port >= 65500 {
            49152
        } else {
            self.next_port + 1
        };
        port
    }

    fn handle_cmd(
        &mut self,
        cmd: ReactorCmd,
        iface: &mut Interface,
        sockets: &mut SocketSet<'static>,
    ) {
        match cmd {
            ReactorCmd::Wake => {}
            ReactorCmd::Resolve { domain, reply } => {
                if let Ok(ip) = super::socks5::strip_brackets(&domain).parse::<IpAddr>() {
                    if self.parsed.allows(ip) {
                        let _ = reply.send(Ok(vec![ip]));
                    } else {
                        let _ = reply.send(Err(RouteError::Blocked(
                            "destination address is outside WireGuard AllowedIPs".into(),
                        )));
                    }
                    return;
                }
                self.start_dns(&domain, DnsAction::Resolve(reply), iface, sockets);
            }
            ReactorCmd::Connect { host, port, reply } => {
                let clean = super::socks5::strip_brackets(&host);
                if let Ok(ip) = clean.parse::<IpAddr>() {
                    self.open_tcp(ip, port, reply, iface, sockets);
                } else {
                    self.start_dns(clean, DnsAction::Connect { port, reply }, iface, sockets);
                }
            }
        }
    }

    fn start_dns(
        &mut self,
        domain: &str,
        action: DnsAction,
        iface: &mut Interface,
        sockets: &mut SocketSet<'static>,
    ) {
        let Some(dns_handle) = self.dns_handle else {
            fail_dns(
                action,
                RouteError::Blocked(
                    "WireGuard DNS server is required to resolve domain names without leaking local DNS"
                        .into(),
                ),
            );
            return;
        };
        let dns_sock = sockets.get_mut::<dns::Socket>(dns_handle);
        let cx = iface.context();
        let mut handles = Vec::new();
        if self.parsed.has_ipv4()
            && let Ok(q) = dns_sock.start_query(cx, domain, DnsQueryType::A)
        {
            handles.push(q);
        }
        if self.parsed.has_ipv6()
            && let Ok(q) = dns_sock.start_query(cx, domain, DnsQueryType::Aaaa)
        {
            handles.push(q);
        }
        if handles.is_empty() {
            fail_dns(
                action,
                RouteError::Transport("failed to start in-tunnel DNS query".into()),
            );
            return;
        }
        self.pending_dns.push(PendingDns {
            queries: handles,
            results: Vec::new(),
            deadline: StdInstant::now() + DNS_TIMEOUT,
            action,
        });
    }

    fn open_tcp(
        &mut self,
        ip: IpAddr,
        port: u16,
        reply: oneshot::Sender<Result<DuplexStream, RouteError>>,
        iface: &mut Interface,
        sockets: &mut SocketSet<'static>,
    ) {
        if !self.parsed.allows(ip) {
            let _ = reply.send(Err(RouteError::Blocked(
                "destination address is outside WireGuard AllowedIPs".into(),
            )));
            return;
        }
        if (ip.is_ipv4() && !self.parsed.has_ipv4()) || (ip.is_ipv6() && !self.parsed.has_ipv6()) {
            let _ = reply.send(Err(RouteError::Blocked(
                "WireGuard tunnel has no local address for this IP version".into(),
            )));
            return;
        }
        let local_port = self.alloc_port();
        let rx_buf = tcp::SocketBuffer::new(vec![0u8; TCP_BUF_SIZE]);
        let tx_buf = tcp::SocketBuffer::new(vec![0u8; TCP_BUF_SIZE]);
        let mut sock = tcp::Socket::new(rx_buf, tx_buf);
        sock.set_keep_alive(Some(smoltcp::time::Duration::from_secs(30)));
        let remote = IpEndpoint::new(to_smol_ip(ip), port);
        if let Err(e) = sock.connect(iface.context(), remote, local_port) {
            let _ = reply.send(Err(RouteError::Transport(format!(
                "WireGuard TCP connect failed: {e}"
            ))));
            return;
        }
        let handle = sockets.add(sock);
        let (_dummy_tx, from_app_rx) = mpsc::channel(1);
        let (to_app_tx, _dummy_rx) = mpsc::channel(1);
        self.tcp_conns.insert(
            handle,
            TcpBridge {
                connect_reply: Some(reply),
                to_net: VecDeque::new(),
                from_app_rx,
                to_app_tx,
                app_closed: false,
                net_eof_sent: false,
                started: StdInstant::now(),
            },
        );
    }

    fn step(&mut self, iface: &mut Interface, sockets: &mut SocketSet<'static>) {
        // 1. Advance pending in-tunnel DNS queries.
        if let Some(dns_handle) = self.dns_handle {
            let now = StdInstant::now();
            let mut finished = Vec::new();
            let mut remaining = Vec::new();
            for mut item in std::mem::take(&mut self.pending_dns) {
                let dns_sock = sockets.get_mut::<dns::Socket>(dns_handle);
                let mut still_waiting = Vec::new();
                for q in item.queries {
                    match dns_sock.get_query_result(q) {
                        Ok(addrs) => {
                            for addr in addrs {
                                item.results.push(from_smol_ip(addr));
                            }
                        }
                        Err(GetQueryResultError::Pending) => {
                            if now < item.deadline {
                                still_waiting.push(q);
                            } else {
                                dns_sock.cancel_query(q);
                            }
                        }
                        Err(GetQueryResultError::Failed) => {}
                    }
                }
                item.queries = still_waiting;
                if item.queries.is_empty() {
                    finished.push(item);
                } else {
                    remaining.push(item);
                }
            }
            self.pending_dns = remaining;

            for item in finished {
                let allowed: Vec<IpAddr> = item
                    .results
                    .into_iter()
                    .filter(|ip| self.parsed.allows(*ip))
                    .collect();
                match item.action {
                    DnsAction::Resolve(reply) => {
                        if allowed.is_empty() {
                            let _ = reply.send(Err(RouteError::Transport(
                                "WireGuard DNS lookup returned no allowed addresses".into(),
                            )));
                        } else {
                            let _ = reply.send(Ok(allowed));
                        }
                    }
                    DnsAction::Connect { port, reply } => {
                        if let Some(&ip) = allowed.first() {
                            self.open_tcp(ip, port, reply, iface, sockets);
                        } else {
                            let _ = reply.send(Err(RouteError::Transport(
                                "WireGuard DNS lookup failed or address outside AllowedIPs".into(),
                            )));
                        }
                    }
                }
            }
        }

        // 2. Advance TCP connections & pump duplex streams.
        let mut to_remove = Vec::new();
        let handles: Vec<SocketHandle> = self.tcp_conns.keys().copied().collect();
        for handle in handles {
            let Some(bridge) = self.tcp_conns.get_mut(&handle) else {
                continue;
            };
            let sock = sockets.get_mut::<tcp::Socket>(handle);

            // Handshake completion?
            if bridge.connect_reply.is_some() {
                if sock.state() == tcp::State::Established {
                    let (app_end, mut reactor_end) = tokio::io::duplex(DUPLEX_BUF_SIZE);
                    let (from_app_tx, from_app_rx) = mpsc::channel::<Vec<u8>>(32);
                    let (to_app_tx, mut to_app_rx) = mpsc::channel::<Vec<u8>>(32);
                    bridge.from_app_rx = from_app_rx;
                    bridge.to_app_tx = to_app_tx;
                    let wake = self.wake_tx.clone();
                    tokio::spawn(async move {
                        let (mut rd, mut wr) = tokio::io::split(&mut reactor_end);
                        let pump_in = async {
                            let mut buf = vec![0u8; 16_384];
                            while let Ok(n) = rd.read(&mut buf).await {
                                if n == 0 {
                                    break;
                                }
                                if from_app_tx.send(buf[..n].to_vec()).await.is_err() {
                                    break;
                                }
                                let _ = wake.send(ReactorCmd::Wake);
                            }
                            drop(from_app_tx);
                            let _ = wake.send(ReactorCmd::Wake);
                        };
                        let pump_out = async {
                            while let Some(chunk) = to_app_rx.recv().await {
                                if wr.write_all(&chunk).await.is_err() {
                                    break;
                                }
                            }
                            let _ = wr.shutdown().await;
                        };
                        tokio::join!(pump_in, pump_out);
                    });
                    if let Some(reply) = bridge.connect_reply.take() {
                        let _ = reply.send(Ok(app_end));
                    }
                } else if sock.state() == tcp::State::Closed
                    || bridge.started.elapsed() > CONNECT_TIMEOUT
                {
                    if let Some(reply) = bridge.connect_reply.take() {
                        let _ = reply.send(Err(RouteError::Transport(
                            "WireGuard TCP connection refused or timed out".into(),
                        )));
                    }
                    sock.abort();
                    to_remove.push(handle);
                    continue;
                } else {
                    continue;
                }
            }

            // Drain bytes from app channel into `to_net`.
            if !bridge.app_closed {
                while bridge.to_net.len() < TCP_BUF_SIZE {
                    match bridge.from_app_rx.try_recv() {
                        Ok(chunk) => bridge.to_net.extend(chunk),
                        Err(mpsc::error::TryRecvError::Empty) => break,
                        Err(mpsc::error::TryRecvError::Disconnected) => {
                            bridge.app_closed = true;
                            break;
                        }
                    }
                }
            }

            // Write `to_net` into smoltcp TCP send buffer (loop to drain both VecDeque slices).
            while sock.can_send() && !bridge.to_net.is_empty() {
                let (head, _) = bridge.to_net.as_slices();
                match sock.send_slice(head) {
                    Ok(0) | Err(_) => break,
                    Ok(sent) => {
                        bridge.to_net.drain(..sent);
                    }
                }
            }
            if bridge.app_closed && bridge.to_net.is_empty() && sock.may_send() {
                sock.close();
            }

            // Read from smoltcp TCP recv buffer into `to_app_tx`.
            while sock.can_recv() {
                let Ok(permit) = bridge.to_app_tx.try_reserve() else {
                    break;
                };
                let data = sock.recv(|buf| {
                    let take = buf.len().min(16_384);
                    (take, buf[..take].to_vec())
                });
                match data {
                    Ok(bytes) if !bytes.is_empty() => permit.send(bytes),
                    _ => break,
                }
            }

            if !sock.may_recv() && !bridge.net_eof_sent {
                bridge.net_eof_sent = true;
                // Replacing sender drops the old channel so pump_out sees EOF and shuts down.
                let (dead_tx, _) = mpsc::channel(1);
                bridge.to_app_tx = dead_tx;
            }

            if sock.state() == tcp::State::Closed {
                to_remove.push(handle);
            }
        }

        for handle in to_remove {
            self.tcp_conns.remove(&handle);
            sockets.remove(handle);
        }
    }
}

fn fail_dns(action: DnsAction, err: RouteError) {
    match action {
        DnsAction::Resolve(reply) => {
            let _ = reply.send(Err(err));
        }
        DnsAction::Connect { reply, .. } => {
            let _ = reply.send(Err(err));
        }
    }
}

/// In-memory Layer-3 IP device connecting `smoltcp` to `boringtun`.
struct WgDevice {
    mtu: usize,
    rx_queue: VecDeque<Vec<u8>>,
    tx_queue: VecDeque<Vec<u8>>,
}

impl WgDevice {
    fn new(mtu: usize) -> Self {
        Self {
            mtu,
            rx_queue: VecDeque::new(),
            tx_queue: VecDeque::new(),
        }
    }
}

struct WgRxToken(Vec<u8>);

impl RxToken for WgRxToken {
    fn consume<R, F>(self, f: F) -> R
    where
        F: FnOnce(&[u8]) -> R,
    {
        f(&self.0)
    }
}

struct WgTxToken<'a>(&'a mut VecDeque<Vec<u8>>);

impl<'a> TxToken for WgTxToken<'a> {
    fn consume<R, F>(self, len: usize, f: F) -> R
    where
        F: FnOnce(&mut [u8]) -> R,
    {
        let mut buf = vec![0u8; len];
        let res = f(&mut buf);
        self.0.push_back(buf);
        res
    }
}

impl Device for WgDevice {
    type RxToken<'a> = WgRxToken;
    type TxToken<'a> = WgTxToken<'a>;

    fn receive(
        &mut self,
        _timestamp: SmolInstant,
    ) -> Option<(Self::RxToken<'_>, Self::TxToken<'_>)> {
        let pkt = self.rx_queue.pop_front()?;
        Some((WgRxToken(pkt), WgTxToken(&mut self.tx_queue)))
    }

    fn transmit(&mut self, _timestamp: SmolInstant) -> Option<Self::TxToken<'_>> {
        Some(WgTxToken(&mut self.tx_queue))
    }

    fn capabilities(&self) -> DeviceCapabilities {
        let mut caps = DeviceCapabilities::default();
        caps.medium = Medium::Ip;
        caps.max_transmission_unit = self.mtu;
        caps
    }
}
