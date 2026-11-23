//! RFC 1928 / RFC 1929 SOCKS5h client (remote DNS) and authenticated loopback bridge.

use std::{
    future::Future,
    net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr},
    sync::Arc,
    time::Duration,
};

use tokio::{
    io::{AsyncReadExt, AsyncWriteExt, DuplexStream},
    net::{TcpListener, TcpStream},
    task::JoinHandle,
    time::timeout,
};
use zeroize::Zeroizing;

use super::RouteError;

const SOCKS_VERSION: u8 = 0x05;
const AUTH_NONE: u8 = 0x00;
const AUTH_USERPASS: u8 = 0x02;
const AUTH_NO_ACCEPTABLE: u8 = 0xFF;
const USERPASS_VERSION: u8 = 0x01;
const CMD_CONNECT: u8 = 0x01;
const ATYP_IPV4: u8 = 0x01;
const ATYP_DOMAIN: u8 = 0x03;
const ATYP_IPV6: u8 = 0x04;

/// Username/password credentials for SOCKS5 (RFC 1929).
#[derive(Clone)]
pub struct Socks5Auth {
    pub username: String,
    pub password: Zeroizing<String>,
}

impl std::fmt::Debug for Socks5Auth {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Socks5Auth")
            .field("username", &"[REDACTED]")
            .field("password", &"[REDACTED]")
            .finish()
    }
}

/// Verify that a SOCKS5 proxy is reachable and accepts our auth method/credentials,
/// without opening any third-party destination connection.
pub async fn probe_socks5(
    proxy_host: &str,
    proxy_port: u16,
    auth: Option<&Socks5Auth>,
    limit: Duration,
) -> Result<(), RouteError> {
    let mut stream = connect_proxy(proxy_host, proxy_port, limit).await?;
    timeout(limit, handshake(&mut stream, auth))
        .await
        .map_err(|_| RouteError::Transport("timed out waiting for SOCKS5 handshake".into()))??;
    let _ = stream.shutdown().await;
    Ok(())
}

/// Connect to `(target_host, target_port)` through a SOCKS5h proxy using remote DNS
/// (`ATYP=0x03` for hostnames, `0x01` for IPv4, `0x04` for IPv6). Never resolves
/// `target_host` locally.
pub async fn connect_socks5(
    proxy_host: &str,
    proxy_port: u16,
    auth: Option<&Socks5Auth>,
    target_host: &str,
    target_port: u16,
    limit: Duration,
) -> Result<TcpStream, RouteError> {
    let mut stream = connect_proxy(proxy_host, proxy_port, limit).await?;
    timeout(limit, async {
        handshake(&mut stream, auth).await?;
        send_connect(&mut stream, target_host, target_port).await?;
        read_reply(&mut stream).await
    })
    .await
    .map_err(|_| RouteError::Transport("SOCKS5 connection timed out".into()))??;
    Ok(stream)
}

async fn connect_proxy(
    proxy_host: &str,
    proxy_port: u16,
    limit: Duration,
) -> Result<TcpStream, RouteError> {
    let host = strip_brackets(proxy_host);
    let connect = async {
        if let Ok(ip) = host.parse::<IpAddr>() {
            TcpStream::connect(SocketAddr::new(ip, proxy_port)).await
        } else {
            TcpStream::connect((host, proxy_port)).await
        }
    };
    let stream = timeout(limit, connect)
        .await
        .map_err(|_| RouteError::Transport("connecting to SOCKS5 proxy timed out".into()))?
        .map_err(|e| RouteError::Transport(format!("connecting to SOCKS5 proxy failed: {e}")))?;
    let _ = stream.set_nodelay(true);
    Ok(stream)
}

async fn handshake(stream: &mut TcpStream, auth: Option<&Socks5Auth>) -> Result<(), RouteError> {
    let method = if auth.is_some() {
        AUTH_USERPASS
    } else {
        AUTH_NONE
    };
    stream
        .write_all(&[SOCKS_VERSION, 1, method])
        .await
        .map_err(|e| RouteError::Transport(format!("SOCKS5 greeting failed: {e}")))?;
    let mut reply = [0u8; 2];
    stream
        .read_exact(&mut reply)
        .await
        .map_err(|e| RouteError::Transport(format!("SOCKS5 greeting reply failed: {e}")))?;
    if reply[0] != SOCKS_VERSION {
        return Err(RouteError::Transport(
            "unexpected SOCKS protocol version".into(),
        ));
    }
    if reply[1] == AUTH_NO_ACCEPTABLE {
        return Err(RouteError::Transport(
            "SOCKS5 proxy rejected the authentication method".into(),
        ));
    }
    if reply[1] != method {
        return Err(RouteError::Transport(
            "SOCKS5 proxy selected an unexpected authentication method".into(),
        ));
    }
    if let Some(auth) = auth {
        let user = auth.username.as_bytes();
        let pass = auth.password.as_bytes();
        if user.len() > 255 || pass.len() > 255 {
            return Err(RouteError::Config(
                "SOCKS5 username and password must each be 255 bytes or fewer".into(),
            ));
        }
        let mut msg = Zeroizing::new(Vec::with_capacity(3 + user.len() + pass.len()));
        msg.push(USERPASS_VERSION);
        msg.push(user.len() as u8);
        msg.extend_from_slice(user);
        msg.push(pass.len() as u8);
        msg.extend_from_slice(pass);
        stream.write_all(&msg).await.map_err(|e| {
            RouteError::Transport(format!("SOCKS5 authentication write failed: {e}"))
        })?;
        let mut status = [0u8; 2];
        stream.read_exact(&mut status).await.map_err(|e| {
            RouteError::Transport(format!("SOCKS5 authentication reply failed: {e}"))
        })?;
        if status[0] != USERPASS_VERSION || status[1] != 0x00 {
            return Err(RouteError::Transport(
                "SOCKS5 proxy rejected the username or password".into(),
            ));
        }
    }
    Ok(())
}

async fn send_connect(
    stream: &mut TcpStream,
    target_host: &str,
    target_port: u16,
) -> Result<(), RouteError> {
    let host = strip_brackets(target_host);
    let mut req = Vec::with_capacity(7 + host.len());
    req.extend_from_slice(&[SOCKS_VERSION, CMD_CONNECT, 0x00]);
    if let Ok(ip) = host.parse::<Ipv4Addr>() {
        req.push(ATYP_IPV4);
        req.extend_from_slice(&ip.octets());
    } else if let Ok(ip) = host.parse::<Ipv6Addr>() {
        req.push(ATYP_IPV6);
        req.extend_from_slice(&ip.octets());
    } else {
        let bytes = host.as_bytes();
        if bytes.is_empty() || bytes.len() > 255 {
            return Err(RouteError::Config("invalid destination hostname".into()));
        }
        req.push(ATYP_DOMAIN);
        req.push(bytes.len() as u8);
        req.extend_from_slice(bytes);
    }
    req.extend_from_slice(&target_port.to_be_bytes());
    stream
        .write_all(&req)
        .await
        .map_err(|e| RouteError::Transport(format!("SOCKS5 connect request failed: {e}")))?;
    Ok(())
}

async fn read_reply(stream: &mut TcpStream) -> Result<(), RouteError> {
    let mut head = [0u8; 4];
    stream
        .read_exact(&mut head)
        .await
        .map_err(|e| RouteError::Transport(format!("SOCKS5 connect reply failed: {e}")))?;
    if head[0] != SOCKS_VERSION {
        return Err(RouteError::Transport(
            "unexpected SOCKS reply version".into(),
        ));
    }
    if head[1] != 0x00 {
        return Err(RouteError::Transport(socks_reply_message(head[1]).into()));
    }
    match head[3] {
        ATYP_IPV4 => {
            let mut rest = [0u8; 6];
            stream
                .read_exact(&mut rest)
                .await
                .map_err(|e| RouteError::Transport(format!("SOCKS5 reply truncated: {e}")))?;
        }
        ATYP_IPV6 => {
            let mut rest = [0u8; 18];
            stream
                .read_exact(&mut rest)
                .await
                .map_err(|e| RouteError::Transport(format!("SOCKS5 reply truncated: {e}")))?;
        }
        ATYP_DOMAIN => {
            let mut len = [0u8; 1];
            stream
                .read_exact(&mut len)
                .await
                .map_err(|e| RouteError::Transport(format!("SOCKS5 reply truncated: {e}")))?;
            let mut rest = vec![0u8; usize::from(len[0]) + 2];
            stream
                .read_exact(&mut rest)
                .await
                .map_err(|e| RouteError::Transport(format!("SOCKS5 reply truncated: {e}")))?;
        }
        _ => {
            return Err(RouteError::Transport(
                "SOCKS5 proxy sent an unknown address type".into(),
            ));
        }
    }
    Ok(())
}

fn socks_reply_message(code: u8) -> &'static str {
    match code {
        0x01 => "SOCKS5 proxy reported a general server failure",
        0x02 => "SOCKS5 proxy refused the connection by ruleset",
        0x03 => "SOCKS5 proxy reported the destination network is unreachable",
        0x04 => "SOCKS5 proxy reported the destination host is unreachable",
        0x05 => "SOCKS5 proxy reported the destination refused the connection",
        0x06 => "SOCKS5 proxy reported TTL expired",
        0x07 => "SOCKS5 proxy does not support this command",
        0x08 => "SOCKS5 proxy does not support this address type",
        _ => "SOCKS5 proxy rejected the connection",
    }
}

pub(crate) fn strip_brackets(host: &str) -> &str {
    host.strip_prefix('[')
        .and_then(|s| s.strip_suffix(']'))
        .unwrap_or(host)
}

/// Localhost-only authenticated SOCKS5h server bridging `reqwest` into the userspace
/// WireGuard tunnel. Bound to `127.0.0.1:0` with a random per-tunnel username & password
/// so no other local process can use it.
pub(crate) struct LoopbackSocks5 {
    addr: SocketAddr,
    username: Zeroizing<String>,
    password: Zeroizing<String>,
    accept_task: JoinHandle<()>,
}

impl LoopbackSocks5 {
    pub(crate) async fn start<F, Fut>(connector: F) -> Result<Self, RouteError>
    where
        F: Fn(String, u16) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<DuplexStream, RouteError>> + Send + 'static,
    {
        let listener = TcpListener::bind(SocketAddr::from((Ipv4Addr::LOCALHOST, 0)))
            .await
            .map_err(|e| RouteError::Transport(format!("bind local tunnel bridge: {e}")))?;
        let addr = listener
            .local_addr()
            .map_err(|e| RouteError::Transport(format!("read local tunnel bridge address: {e}")))?;
        let mut raw = Zeroizing::new([0u8; 32]);
        crate::identity::random_bytes(&mut *raw)
            .map_err(|e| RouteError::Transport(format!("generate tunnel bridge secret: {e}")))?;
        let username = Zeroizing::new(hex_encode(&raw[..16]));
        let password = Zeroizing::new(hex_encode(&raw[16..]));
        let expected_user = username.clone();
        let expected_pass = password.clone();
        let connector = Arc::new(connector);

        let accept_task = tokio::spawn(async move {
            loop {
                let Ok((stream, _)) = listener.accept().await else {
                    break;
                };
                let user = expected_user.clone();
                let pass = expected_pass.clone();
                let connector = connector.clone();
                tokio::spawn(async move {
                    let _ = handle_bridge_client(stream, &user, &pass, connector).await;
                });
            }
        });

        Ok(Self {
            addr,
            username,
            password,
            accept_task,
        })
    }

    pub(crate) fn reqwest_proxy(&self) -> Result<reqwest::Proxy, RouteError> {
        let url = format!("socks5h://127.0.0.1:{}", self.addr.port());
        reqwest::Proxy::all(&url)
            .map(|proxy| proxy.basic_auth(self.username.as_str(), self.password.as_str()))
            .map_err(|e| RouteError::Transport(format!("build local tunnel proxy: {e}")))
    }

    /// `socks5://` variant for callers that pre-resolved & validated destination IPs
    /// inside the tunnel and pinned them with a strict custom `reqwest::dns::Resolve`,
    /// preventing a second in-tunnel DNS lookup (DNS rebinding).
    pub(crate) fn reqwest_proxy_resolved(&self) -> Result<reqwest::Proxy, RouteError> {
        let url = format!("socks5://127.0.0.1:{}", self.addr.port());
        reqwest::Proxy::all(&url)
            .map(|proxy| proxy.basic_auth(self.username.as_str(), self.password.as_str()))
            .map_err(|e| RouteError::Transport(format!("build local resolved tunnel proxy: {e}")))
    }
}

impl Drop for LoopbackSocks5 {
    fn drop(&mut self) {
        self.accept_task.abort();
    }
}

const BRIDGE_HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(5);

async fn read_bridge_request(
    stream: &mut TcpStream,
    expected_user: &str,
    expected_pass: &str,
) -> Result<(String, u16), ()> {
    let mut head = [0u8; 2];
    stream.read_exact(&mut head).await.map_err(|_| ())?;
    if head[0] != SOCKS_VERSION || head[1] == 0 {
        return Err(());
    }
    let mut methods = vec![0u8; usize::from(head[1])];
    stream.read_exact(&mut methods).await.map_err(|_| ())?;
    if !methods.contains(&AUTH_USERPASS) {
        let _ = stream.write_all(&[SOCKS_VERSION, AUTH_NO_ACCEPTABLE]).await;
        return Err(());
    }
    stream
        .write_all(&[SOCKS_VERSION, AUTH_USERPASS])
        .await
        .map_err(|_| ())?;

    let mut auth_head = [0u8; 2];
    stream.read_exact(&mut auth_head).await.map_err(|_| ())?;
    if auth_head[0] != USERPASS_VERSION {
        return Err(());
    }
    let mut user = Zeroizing::new(vec![0u8; usize::from(auth_head[1])]);
    stream.read_exact(&mut user).await.map_err(|_| ())?;
    let mut pass_len = [0u8; 1];
    stream.read_exact(&mut pass_len).await.map_err(|_| ())?;
    let mut pass = Zeroizing::new(vec![0u8; usize::from(pass_len[0])]);
    stream.read_exact(&mut pass).await.map_err(|_| ())?;

    if !ct_eq(&user, expected_user.as_bytes()) || !ct_eq(&pass, expected_pass.as_bytes()) {
        let _ = stream.write_all(&[USERPASS_VERSION, 0x01]).await;
        return Err(());
    }
    stream
        .write_all(&[USERPASS_VERSION, 0x00])
        .await
        .map_err(|_| ())?;

    let mut req = [0u8; 4];
    stream.read_exact(&mut req).await.map_err(|_| ())?;
    if req[0] != SOCKS_VERSION || req[1] != CMD_CONNECT {
        let _ = write_bridge_reply(stream, 0x07).await;
        return Err(());
    }
    let target_host = match req[3] {
        ATYP_IPV4 => {
            let mut addr = [0u8; 4];
            stream.read_exact(&mut addr).await.map_err(|_| ())?;
            Ipv4Addr::from(addr).to_string()
        }
        ATYP_IPV6 => {
            let mut addr = [0u8; 16];
            stream.read_exact(&mut addr).await.map_err(|_| ())?;
            Ipv6Addr::from(addr).to_string()
        }
        ATYP_DOMAIN => {
            let mut len = [0u8; 1];
            stream.read_exact(&mut len).await.map_err(|_| ())?;
            let mut domain = vec![0u8; usize::from(len[0])];
            stream.read_exact(&mut domain).await.map_err(|_| ())?;
            String::from_utf8(domain).map_err(|_| ())?
        }
        _ => {
            let _ = write_bridge_reply(stream, 0x08).await;
            return Err(());
        }
    };
    let mut port_bytes = [0u8; 2];
    stream.read_exact(&mut port_bytes).await.map_err(|_| ())?;
    let target_port = u16::from_be_bytes(port_bytes);
    Ok((target_host, target_port))
}

async fn handle_bridge_client<F, Fut>(
    mut stream: TcpStream,
    expected_user: &str,
    expected_pass: &str,
    connector: Arc<F>,
) -> Result<(), ()>
where
    F: Fn(String, u16) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = Result<DuplexStream, RouteError>> + Send + 'static,
{
    let _ = stream.set_nodelay(true);
    let (target_host, target_port) = tokio::time::timeout(
        BRIDGE_HANDSHAKE_TIMEOUT,
        read_bridge_request(&mut stream, expected_user, expected_pass),
    )
    .await
    .map_err(|_| ())??;

    match connector(target_host, target_port).await {
        Ok(mut tunnel) => {
            write_bridge_reply(&mut stream, 0x00)
                .await
                .map_err(|_| ())?;
            let _ = tokio::io::copy_bidirectional(&mut stream, &mut tunnel).await;
            Ok(())
        }
        Err(_) => {
            let _ = write_bridge_reply(&mut stream, 0x05).await;
            Err(())
        }
    }
}

async fn write_bridge_reply(stream: &mut TcpStream, code: u8) -> std::io::Result<()> {
    stream
        .write_all(&[SOCKS_VERSION, code, 0x00, ATYP_IPV4, 0, 0, 0, 0, 0, 0])
        .await
}

fn ct_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (&x, &y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

fn hex_encode(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for &b in bytes {
        out.push(HEX[usize::from(b >> 4)] as char);
        out.push(HEX[usize::from(b & 0x0f)] as char);
    }
    out
}
