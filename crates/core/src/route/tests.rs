//! Controlled-endpoint verification tests for Direct, SOCKS5, WireGuard, and Tor routing.
//!
//! Spins up real local SOCKS5h proxies (IPv4, IPv6, domain remote DNS, auth), a real
//! `boringtun` + `smoltcp` WireGuard peer (Noise_IKpsk2 handshake, in-tunnel UDP DNS,
//! dual-stack IPv4/IPv6 TCP, pinned TLS), and pinned-TLS HTTPS + WebSocket endpoints.

use std::{
    collections::HashMap,
    net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr},
    sync::{
        Arc, LazyLock, Mutex as StdMutex,
        atomic::{AtomicUsize, Ordering},
    },
    time::{Duration, Instant as StdInstant},
};

use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64};
use boringtun::{
    noise::{Tunn, TunnResult},
    x25519::{PublicKey, StaticSecret},
};
use futures_util::{SinkExt, StreamExt};
use rustls::{ServerConfig, ServerConnection, crypto::ring, pki_types::PrivatePkcs8KeyDer};
use smoltcp::{
    iface::{Config as IfaceConfig, Interface, SocketSet},
    phy::{Device, DeviceCapabilities, Medium, RxToken, TxToken},
    socket::{tcp, udp},
    time::Instant as SmolInstant,
    wire::{HardwareAddress, IpAddress, IpCidr},
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream, UdpSocket},
    sync::oneshot,
};
use tokio_rustls::TlsAcceptor;
use tokio_tungstenite::{Connector, tungstenite::Message};
use zeroize::Zeroizing;

use super::*;
use crate::pinned_tls::{self, fingerprint, is_pin_mismatch};

/// Serialize tests that mutate the process-global route controller.
static TEST_LOCK: LazyLock<tokio::sync::Mutex<()>> = LazyLock::new(|| tokio::sync::Mutex::new(()));

fn reset_direct() {
    let _ = apply(RouteConfig::default(), RouteSecrets::default());
}

struct PinnedTarget {
    addr: SocketAddr,
    pin: crate::auth::Fingerprint,
    wrong_pin: crate::auth::Fingerprint,
    hits: Arc<AtomicUsize>,
    _stop: oneshot::Sender<()>,
}

/// Start a real pinned-TLS HTTPS (+ optional WebSocket upgrade) target on `bind`.
async fn start_pinned_https(bind: SocketAddr) -> PinnedTarget {
    let cert = rcgen::generate_simple_self_signed(vec![
        "localhost".into(),
        "rusttalk-test.invalid".into(),
        "rusttalktest1234567890.onion".into(),
    ])
    .unwrap();
    let other = rcgen::generate_simple_self_signed(vec!["localhost".into()]).unwrap();
    let pin = fingerprint(cert.cert.der().as_ref());
    let wrong_pin = fingerprint(other.cert.der().as_ref());
    let server_cfg = Arc::new(
        ServerConfig::builder_with_provider(Arc::new(ring::default_provider()))
            .with_safe_default_protocol_versions()
            .unwrap()
            .with_no_client_auth()
            .with_single_cert(
                vec![cert.cert.der().clone()],
                PrivatePkcs8KeyDer::from(cert.signing_key.serialize_der()).into(),
            )
            .unwrap(),
    );
    let acceptor = TlsAcceptor::from(server_cfg);
    let listener = TcpListener::bind(bind).await.unwrap();
    let addr = listener.local_addr().unwrap();
    let hits = Arc::new(AtomicUsize::new(0));
    let hits_task = hits.clone();
    let (stop_tx, mut stop_rx) = oneshot::channel();
    tokio::spawn(async move {
        loop {
            tokio::select! {
                _ = &mut stop_rx => break,
                accepted = listener.accept() => {
                    let Ok((stream, _)) = accepted else { break };
                    hits_task.fetch_add(1, Ordering::SeqCst);
                    let acceptor = acceptor.clone();
                    tokio::spawn(async move {
                        let Ok(mut tls) = acceptor.accept(stream).await else { return };
                        let mut buf = [0u8; 2048];
                        let Ok(n) = tls.read(&mut buf).await else { return };
                        let req = String::from_utf8_lossy(&buf[..n]);
                        if req.to_ascii_lowercase().contains("upgrade: websocket") {
                            // Minimal RFC 6455 accept for WS verification.
                            let key = req.lines().find_map(|l| {
                                let (k, v) = l.split_once(':')?;
                                k.trim().eq_ignore_ascii_case("sec-websocket-key").then(|| v.trim())
                            });
                            if let Some(key) = key {
                                let accept = sha1_accept(key);
                                let reply = format!(
                                    "HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Accept: {accept}\r\n\r\n"
                                );
                                let _ = tls.write_all(reply.as_bytes()).await;
                                let _ = tls.flush().await;
                                let mut ws = tokio_tungstenite::WebSocketStream::from_raw_socket(
                                    tls,
                                    tokio_tungstenite::tungstenite::protocol::Role::Server,
                                    None,
                                )
                                .await;
                                if let Some(Ok(Message::Text(text))) = ws.next().await {
                                    let _ = ws.send(Message::Text(format!("echo:{text}").into())).await;
                                }
                                let _ = ws.close(None).await;
                            }
                            return;
                        }
                        let body = "routed-ok";
                        let reply = format!(
                            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                            body.len()
                        );
                        let _ = tls.write_all(reply.as_bytes()).await;
                        let _ = tls.shutdown().await;
                    });
                }
            }
        }
    });
    PinnedTarget {
        addr,
        pin,
        wrong_pin,
        hits,
        _stop: stop_tx,
    }
}

/// RFC 6455 Sec-WebSocket-Accept (SHA-1 via `ring` digest? We compute SHA-1 cleanly in 35 lines
/// so no extra crate is needed for the test helper!).
fn sha1_accept(key: &str) -> String {
    let mut input = format!("{key}258EAFA5-E914-47DA-95CA-C5AB0DC85B11").into_bytes();
    let bit_len = (input.len() as u64) * 8;
    input.push(0x80);
    while input.len() % 64 != 56 {
        input.push(0);
    }
    input.extend_from_slice(&bit_len.to_be_bytes());
    let mut h: [u32; 5] = [0x67452301, 0xEFCDAB89, 0x98BADCFE, 0x10325476, 0xC3D2E1F0];
    for chunk in input.as_chunks::<64>().0 {
        let mut w = [0u32; 80];
        for i in 0..16 {
            w[i] = u32::from_be_bytes(chunk[i * 4..i * 4 + 4].try_into().unwrap());
        }
        for i in 16..80 {
            w[i] = (w[i - 3] ^ w[i - 8] ^ w[i - 14] ^ w[i - 16]).rotate_left(1);
        }
        let [mut a, mut b, mut c, mut d, mut e] = h;
        for (i, &wi) in w.iter().enumerate() {
            let (f, k) = match i {
                0..=19 => ((b & c) | ((!b) & d), 0x5A827999),
                20..=39 => (b ^ c ^ d, 0x6ED9EBA1),
                40..=59 => ((b & c) | (b & d) | (c & d), 0x8F1BBCDC),
                _ => (b ^ c ^ d, 0xCA62C1D6),
            };
            let temp = a
                .rotate_left(5)
                .wrapping_add(f)
                .wrapping_add(e)
                .wrapping_add(k)
                .wrapping_add(wi);
            e = d;
            d = c;
            c = b.rotate_left(30);
            b = a;
            a = temp;
        }
        h[0] = h[0].wrapping_add(a);
        h[1] = h[1].wrapping_add(b);
        h[2] = h[2].wrapping_add(c);
        h[3] = h[3].wrapping_add(d);
        h[4] = h[4].wrapping_add(e);
    }
    let mut out = [0u8; 20];
    for (i, word) in h.iter().enumerate() {
        out[i * 4..i * 4 + 4].copy_from_slice(&word.to_be_bytes());
    }
    BASE64.encode(out)
}

#[derive(Clone, Debug)]
struct ObservedConnect {
    atyp: u8,
    host: String,
    port: u16,
}

struct MockSocks5 {
    addr: SocketAddr,
    observed: Arc<StdMutex<Vec<ObservedConnect>>>,
    stop: Option<oneshot::Sender<()>>,
}

impl MockSocks5 {
    fn shutdown(&mut self) {
        if let Some(tx) = self.stop.take() {
            let _ = tx.send(());
        }
    }
}

/// Start a controlled SOCKS5h proxy recording every ATYP + target host and optionally
/// enforcing RFC 1929 auth and remapping synthetic domains (e.g. `.invalid`, `.onion`).
async fn start_mock_socks5(
    required_auth: Option<(String, String)>,
    domain_map: HashMap<String, SocketAddr>,
) -> MockSocks5 {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let observed = Arc::new(StdMutex::new(Vec::new()));
    let observed_task = observed.clone();
    let domain_map = Arc::new(domain_map);
    let (stop_tx, mut stop_rx) = oneshot::channel();
    tokio::spawn(async move {
        loop {
            tokio::select! {
                _ = &mut stop_rx => break,
                accepted = listener.accept() => {
                    let Ok((mut client, _)) = accepted else { break };
                    let auth = required_auth.clone();
                    let observed = observed_task.clone();
                    let domain_map = domain_map.clone();
                    tokio::spawn(async move {
                        let mut head = [0u8; 2];
                        if client.read_exact(&mut head).await.is_err() || head[0] != 5 {
                            return;
                        }
                        let mut methods = vec![0u8; usize::from(head[1])];
                        if client.read_exact(&mut methods).await.is_err() {
                            return;
                        }
                        if let Some((want_u, want_p)) = auth {
                            if !methods.contains(&2) {
                                let _ = client.write_all(&[5, 0xFF]).await;
                                return;
                            }
                            let _ = client.write_all(&[5, 2]).await;
                            let mut ah = [0u8; 2];
                            if client.read_exact(&mut ah).await.is_err() { return; }
                            let mut u = vec![0u8; usize::from(ah[1])];
                            if client.read_exact(&mut u).await.is_err() { return; }
                            let mut pl = [0u8; 1];
                            if client.read_exact(&mut pl).await.is_err() { return; }
                            let mut p = vec![0u8; usize::from(pl[0])];
                            if client.read_exact(&mut p).await.is_err() { return; }
                            if u != want_u.as_bytes() || p != want_p.as_bytes() {
                                let _ = client.write_all(&[1, 1]).await;
                                return;
                            }
                            let _ = client.write_all(&[1, 0]).await;
                        } else {
                            let _ = client.write_all(&[5, 0]).await;
                        }
                        let mut req = [0u8; 4];
                        if client.read_exact(&mut req).await.is_err() {
                            // Probe closed cleanly after handshake!
                            return;
                        }
                        let atyp = req[3];
                        let host = match atyp {
                            1 => {
                                let mut b = [0u8; 4];
                                if client.read_exact(&mut b).await.is_err() { return; }
                                Ipv4Addr::from(b).to_string()
                            }
                            4 => {
                                let mut b = [0u8; 16];
                                if client.read_exact(&mut b).await.is_err() { return; }
                                Ipv6Addr::from(b).to_string()
                            }
                            3 => {
                                let mut l = [0u8; 1];
                                if client.read_exact(&mut l).await.is_err() { return; }
                                let mut d = vec![0u8; usize::from(l[0])];
                                if client.read_exact(&mut d).await.is_err() { return; }
                                String::from_utf8_lossy(&d).into_owned()
                            }
                            _ => return,
                        };
                        let mut pb = [0u8; 2];
                        if client.read_exact(&mut pb).await.is_err() { return; }
                        let port = u16::from_be_bytes(pb);
                        observed.lock().unwrap().push(ObservedConnect {
                            atyp,
                            host: host.clone(),
                            port,
                        });
                        let target: SocketAddr = if let Some(&mapped) = domain_map.get(&host) {
                            mapped
                        } else if let Ok(ip) = host.parse::<IpAddr>() {
                            SocketAddr::new(ip, port)
                        } else {
                            let _ = client.write_all(&[5, 4, 0, 1, 0, 0, 0, 0, 0, 0]).await;
                            return;
                        };
                        let Ok(mut upstream) = TcpStream::connect(target).await else {
                            let _ = client.write_all(&[5, 5, 0, 1, 0, 0, 0, 0, 0, 0]).await;
                            return;
                        };
                        let _ = client.write_all(&[5, 0, 0, 1, 127, 0, 0, 1, 0, 0]).await;
                        let _ = tokio::io::copy_bidirectional(&mut client, &mut upstream).await;
                    });
                }
            }
        }
    });
    MockSocks5 {
        addr,
        observed,
        stop: Some(stop_tx),
    }
}

#[test]
fn wireguard_conf_parses_ipv4_ipv6_dns_and_redacts_secrets() {
    let priv_key = BASE64.encode([7u8; 32]);
    let pub_key = BASE64.encode([9u8; 32]);
    let psk = BASE64.encode([3u8; 32]);
    let conf = format!(
        "[Interface]\nPrivateKey = {priv_key}\nAddress = 10.66.0.2/32, fd42::2/128\nDNS = 10.66.0.1, fd42::1\nMTU = 1380\n\n[Peer]\nPublicKey = {pub_key}\nPresharedKey = {psk}\nEndpoint = [2001:db8::1]:51820\nAllowedIPs = 0.0.0.0/0, ::/0\nPersistentKeepalive = 25\n"
    );
    let (cfg, got_priv, got_psk) = WireGuardConfig::parse_conf(&conf).unwrap();
    assert_eq!(cfg.addresses, vec!["10.66.0.2/32", "fd42::2/128"]);
    assert_eq!(cfg.dns, vec!["10.66.0.1", "fd42::1"]);
    assert_eq!(cfg.mtu, 1380);
    assert_eq!(cfg.endpoint, "[2001:db8::1]:51820");
    assert!(cfg.has_private_key && cfg.has_preshared_key);
    assert_eq!(
        got_priv.as_deref().map(String::as_str),
        Some(priv_key.as_str())
    );
    assert_eq!(got_psk.as_deref().map(String::as_str), Some(psk.as_str()));

    let secrets = RouteSecrets {
        socks5_password: Some(Zeroizing::new("super-secret-socks".into())),
        tor_password: Some(Zeroizing::new("super-secret-tor".into())),
        wg_private_key: got_priv,
        wg_preshared_key: got_psk,
    };
    let debug = format!("{secrets:?}");
    assert!(!debug.contains("super-secret"));
    assert!(!debug.contains(&priv_key));
    assert!(!debug.contains(&psk));
}

#[test]
fn missing_credentials_fail_closed_and_block_traffic() {
    let _guard = TEST_LOCK.blocking_lock();
    let cfg = RouteConfig {
        kind: RouteKind::Socks5,
        socks5: Socks5Config {
            host: "127.0.0.1".into(),
            port: 1080,
            username: "alice".into(),
            has_password: true,
        },
        ..Default::default()
    };
    let err = apply(cfg, RouteSecrets::default()).unwrap_err();
    assert!(matches!(err, RouteError::Blocked(_)));
    assert!(matches!(
        snapshot().status,
        RouteStatus::Blocked {
            kind: RouteKind::Socks5,
            ..
        }
    ));
    assert!(http_proxy().is_err());
    assert!(allow_realtime_media("Voice calls").is_err());
    assert!(allow_sip().is_err());
    reset_direct();
}

#[test]
fn pending_onboarding_blocks_all_outbound_traffic() {
    let _guard = TEST_LOCK.blocking_lock();
    let snap = enter_pending();
    assert_eq!(snap.status, RouteStatus::Pending);
    assert!(http_proxy().is_err());
    assert!(allow_realtime_media("Voice calls").is_err());
    assert!(allow_sip().is_err());
    reset_direct();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn socks5h_routes_https_ws_remote_dns_pins_and_blocks_on_failure_without_direct_fallback() {
    let _guard = TEST_LOCK.lock().await;
    let target = start_pinned_https("127.0.0.1:0".parse().unwrap()).await;
    let mut map = HashMap::new();
    map.insert("rusttalk-test.invalid".to_owned(), target.addr);
    let mut proxy = start_mock_socks5(Some(("alice".into(), "p@ss:word/1".into())), map).await;

    // 1. Wrong password fails closed -> Blocked, zero direct hits.
    let bad_cfg = RouteConfig {
        kind: RouteKind::Socks5,
        socks5: Socks5Config {
            host: "127.0.0.1".into(),
            port: proxy.addr.port(),
            username: "alice".into(),
            has_password: true,
        },
        ..Default::default()
    };
    let bad_sec = RouteSecrets {
        socks5_password: Some(Zeroizing::new("wrong-password".into())),
        ..Default::default()
    };
    assert!(apply(bad_cfg.clone(), bad_sec).is_err());
    assert!(matches!(
        snapshot().status,
        RouteStatus::Blocked {
            kind: RouteKind::Socks5,
            ..
        }
    ));
    assert!(pinned_tls::https_client(Some(target.pin)).is_err());
    assert_eq!(target.hits.load(Ordering::SeqCst), 0);

    // 2. Correct password activates SOCKS5h.
    let good_sec = RouteSecrets {
        socks5_password: Some(Zeroizing::new("p@ss:word/1".into())),
        ..Default::default()
    };
    let snap = apply(bad_cfg, good_sec).unwrap();
    assert!(matches!(
        snap.status,
        RouteStatus::Active {
            kind: RouteKind::Socks5,
            ..
        }
    ));

    // 3. HTTPS to synthetic `.invalid` domain proves ZERO local DNS lookup (ATYP=0x03) + pin preserved.
    let client = pinned_tls::https_client(Some(target.pin)).unwrap();
    let body = client
        .get(format!(
            "https://rusttalk-test.invalid:{}/",
            target.addr.port()
        ))
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert_eq!(body, "routed-ok");
    let obs = proxy.observed.lock().unwrap().clone();
    assert!(
        obs.iter().any(|c| c.atyp == 3
            && c.host == "rusttalk-test.invalid"
            && c.port == target.addr.port())
    );

    // 4. Wrong TLS pin STILL fails with typed PinMismatch through SOCKS5h.
    let wrong_client = pinned_tls::https_client(Some(target.wrong_pin)).unwrap();
    let err = wrong_client
        .get(format!(
            "https://rusttalk-test.invalid:{}/",
            target.addr.port()
        ))
        .send()
        .await
        .unwrap_err();
    assert!(is_pin_mismatch(&err), "{err:?}");

    // 5. WebSocket over SOCKS5h (`route::connect_tcp` + pinned rustls) succeeds with ATYP=0x03.
    let stream = connect_tcp(
        "rusttalk-test.invalid",
        target.addr.port(),
        Duration::from_secs(5),
    )
    .await
    .unwrap();
    let tls_cfg = Arc::new(pinned_tls_test_config(target.pin));
    let (mut ws, _) = tokio_tungstenite::client_async_tls_with_config(
        format!(
            "wss://rusttalk-test.invalid:{}/v1/events",
            target.addr.port()
        ),
        stream,
        None,
        Some(Connector::Rustls(tls_cfg)),
    )
    .await
    .unwrap();
    ws.send(Message::Text("hello".into())).await.unwrap();
    let reply = ws.next().await.unwrap().unwrap();
    assert_eq!(reply, Message::Text("echo:hello".into()));

    // 5b. IPv6 target via SOCKS5h (`ATYP=0x04`) when loopback IPv6 is enabled on this host.
    if let Ok(v6_listener) = TcpListener::bind("[::1]:0").await {
        let v6_addr = v6_listener.local_addr().unwrap();
        let v6_task = tokio::spawn(async move {
            if let Ok((mut s, _)) = v6_listener.accept().await {
                let mut b = [0u8; 4];
                let _ = s.read_exact(&mut b).await;
                let _ = s.write_all(b"v6ok").await;
            }
        });
        let mut s = connect_tcp("::1", v6_addr.port(), Duration::from_secs(5))
            .await
            .unwrap();
        s.write_all(b"ping").await.unwrap();
        let mut reply = [0u8; 4];
        s.read_exact(&mut reply).await.unwrap();
        assert_eq!(&reply, b"v6ok");
        let _ = v6_task.await;
        let obs = proxy.observed.lock().unwrap().clone();
        assert!(
            obs.iter()
                .any(|c| c.atyp == 4 && c.host == "::1" && c.port == v6_addr.port())
        );
    }

    // 6. Voice calls, live transfers, and SIP are explicitly blocked under SOCKS5.
    assert!(matches!(
        allow_realtime_media("Voice calls"),
        Err(RouteError::Unsupported {
            kind: RouteKind::Socks5,
            ..
        })
    ));
    assert!(matches!(
        allow_sip(),
        Err(RouteError::Unsupported {
            kind: RouteKind::Socks5,
            ..
        })
    ));

    // 7. Stop proxy mid-session -> traffic fails cleanly with ZERO direct fallback to target!
    let hits_before = target.hits.load(Ordering::SeqCst);
    proxy.shutdown();
    tokio::time::sleep(Duration::from_millis(50)).await;
    let direct_addr_url = format!("https://127.0.0.1:{}/", target.addr.port());
    assert!(client.get(&direct_addr_url).send().await.is_err());
    assert_eq!(
        target.hits.load(Ordering::SeqCst),
        hits_before,
        "must never fall back directly to 127.0.0.1 when proxy stops"
    );

    reset_direct();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn tor_socks5h_routes_onion_with_stream_isolation_and_blocks_webrtc_and_sip() {
    let _guard = TEST_LOCK.lock().await;
    let target = start_pinned_https("127.0.0.1:0".parse().unwrap()).await;
    let mut map = HashMap::new();
    map.insert("rusttalktest1234567890.onion".to_owned(), target.addr);
    let proxy = start_mock_socks5(Some(("iso-user".into(), "iso-pass".into())), map).await;

    let cfg = RouteConfig {
        kind: RouteKind::Tor,
        tor: TorConfig {
            host: "127.0.0.1".into(),
            port: proxy.addr.port(),
            username: "iso-user".into(),
            has_password: true,
        },
        ..Default::default()
    };
    let sec = RouteSecrets {
        tor_password: Some(Zeroizing::new("iso-pass".into())),
        ..Default::default()
    };
    let snap = apply(cfg, sec).unwrap();
    assert!(matches!(
        snap.status,
        RouteStatus::Active {
            kind: RouteKind::Tor,
            ..
        }
    ));

    let client = pinned_tls::https_client(Some(target.pin)).unwrap();
    let body = client
        .get(format!(
            "https://rusttalktest1234567890.onion:{}/",
            target.addr.port()
        ))
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert_eq!(body, "routed-ok");
    let obs = proxy.observed.lock().unwrap().clone();
    assert!(
        obs.iter()
            .any(|c| c.atyp == 3 && c.host == "rusttalktest1234567890.onion")
    );
    assert!(matches!(
        allow_realtime_media("Voice calls"),
        Err(RouteError::Unsupported {
            kind: RouteKind::Tor,
            ..
        })
    ));
    assert!(matches!(
        allow_realtime_media("Live file transfers"),
        Err(RouteError::Unsupported {
            kind: RouteKind::Tor,
            ..
        })
    ));
    assert!(matches!(
        allow_sip(),
        Err(RouteError::Unsupported {
            kind: RouteKind::Tor,
            ..
        })
    ));

    reset_direct();
}

/// Real controlled WireGuard peer (`boringtun` + `smoltcp`) serving in-tunnel UDP DNS
/// (A + AAAA) and dual-stack IPv4 + IPv6 pinned-TLS HTTPS!
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn wireguard_userspace_tunnel_routes_ipv4_ipv6_dns_https_pins_and_enforces_allowed_ips() {
    let _guard = TEST_LOCK.lock().await;

    // Generate real X25519 keypairs + PSK for client and controlled WireGuard peer.
    let mut client_priv_bytes = [0u8; 32];
    let mut peer_priv_bytes = [0u8; 32];
    let mut psk_bytes = [0u8; 32];
    crate::identity::random_bytes(&mut client_priv_bytes).unwrap();
    crate::identity::random_bytes(&mut peer_priv_bytes).unwrap();
    crate::identity::random_bytes(&mut psk_bytes).unwrap();
    let client_priv = StaticSecret::from(client_priv_bytes);
    let client_pub = PublicKey::from(&client_priv);
    let peer_priv = StaticSecret::from(peer_priv_bytes);
    let peer_pub = PublicKey::from(&peer_priv);

    let cert =
        rcgen::generate_simple_self_signed(vec!["wg-v4.invalid".into(), "wg-v6.invalid".into()])
            .unwrap();
    let other = rcgen::generate_simple_self_signed(vec!["wg-v4.invalid".into()]).unwrap();
    let pin = fingerprint(cert.cert.der().as_ref());
    let wrong_pin = fingerprint(other.cert.der().as_ref());
    let tls_server_cfg = Arc::new(
        ServerConfig::builder_with_provider(Arc::new(ring::default_provider()))
            .with_safe_default_protocol_versions()
            .unwrap()
            .with_no_client_auth()
            .with_single_cert(
                vec![cert.cert.der().clone()],
                PrivatePkcs8KeyDer::from(cert.signing_key.serialize_der()).into(),
            )
            .unwrap(),
    );

    let peer_udp = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let peer_addr = peer_udp.local_addr().unwrap();
    let encrypted_packets = Arc::new(AtomicUsize::new(0));
    let dns_queries_seen = Arc::new(AtomicUsize::new(0));
    let (stop_peer_tx, stop_peer_rx) = oneshot::channel();
    spawn_controlled_wg_peer(
        peer_udp,
        peer_priv,
        client_pub,
        psk_bytes,
        tls_server_cfg,
        encrypted_packets.clone(),
        dns_queries_seen.clone(),
        stop_peer_rx,
    );

    let cfg = RouteConfig {
        kind: RouteKind::WireGuard,
        wireguard: WireGuardConfig {
            addresses: vec!["10.77.0.2/24".into(), "fd77::2/64".into()],
            dns: vec!["10.77.0.1".into()],
            mtu: 1280,
            peer_public_key: BASE64.encode(peer_pub.as_bytes()),
            endpoint: peer_addr.to_string(),
            allowed_ips: vec!["10.77.0.0/24".into(), "fd77::/64".into()],
            persistent_keepalive: Some(25),
            has_private_key: true,
            has_preshared_key: true,
        },
        ..Default::default()
    };
    let sec = RouteSecrets {
        wg_private_key: Some(Zeroizing::new(BASE64.encode(client_priv_bytes))),
        wg_preshared_key: Some(Zeroizing::new(BASE64.encode(psk_bytes))),
        ..Default::default()
    };

    // 1. Handshake succeeds and activates WireGuard.
    let snap = apply(cfg, sec).unwrap();
    assert!(matches!(
        snap.status,
        RouteStatus::Active {
            kind: RouteKind::WireGuard,
            ..
        }
    ));
    assert!(encrypted_packets.load(Ordering::SeqCst) > 0);

    // 2. IPv4 pinned HTTPS using `.invalid` domain resolved ONLY via in-tunnel WireGuard DNS!
    let client = pinned_tls::https_client(Some(pin)).unwrap();
    let body_v4 = client
        .get("https://wg-v4.invalid:8443/")
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert_eq!(body_v4, "wg-https-ok");
    assert!(dns_queries_seen.load(Ordering::SeqCst) >= 1);

    // 3. IPv6 pinned HTTPS using `.invalid` domain resolved ONLY via in-tunnel WireGuard DNS!
    let body_v6 = client
        .get("https://wg-v6.invalid:8443/")
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert_eq!(body_v6, "wg-https-ok");

    // 4. Wrong TLS pin STILL fails with PinMismatch inside WireGuard!
    let bad_pin_client = pinned_tls::https_client(Some(wrong_pin)).unwrap();
    let err = bad_pin_client
        .get("https://wg-v4.invalid:8443/")
        .send()
        .await
        .unwrap_err();
    assert!(is_pin_mismatch(&err), "{err:?}");

    // 5. Destination outside AllowedIPs (10.88.0.1) is refused, never sent direct!
    let outside = connect_tcp("10.88.0.1", 443, Duration::from_secs(2)).await;
    assert!(matches!(outside, Err(RouteError::Blocked(_))));

    // 6. WebRTC media and SIP are explicitly blocked under app-only WireGuard.
    assert!(matches!(
        allow_realtime_media("Voice calls"),
        Err(RouteError::Unsupported {
            kind: RouteKind::WireGuard,
            ..
        })
    ));
    assert!(matches!(
        allow_sip(),
        Err(RouteError::Unsupported {
            kind: RouteKind::WireGuard,
            ..
        })
    ));

    // 6b. Pre-resolved link-card fetch uses `socks5://` + `PinnedDnsResolver` so the
    // tunnel performs ZERO second DNS lookup (preventing DNS-rebinding SSRF) while
    // keeping original Host header and TLS SNI intact.
    let link_url = url::Url::parse("https://wg-v4.invalid:8443/").unwrap();
    let resolved =
        resolve_for_link_card(&link_url, true, |_| true, Duration::from_secs(5)).unwrap();
    let addrs = match resolved {
        LinkCardResolution::Resolved(addrs) => addrs,
        other => panic!("expected WireGuard Resolved addrs, got {other:?}"),
    };
    assert!(
        addrs
            .iter()
            .any(|a| a.ip() == IpAddr::V4(Ipv4Addr::new(10, 77, 0, 1)))
    );
    let dns_before_fetch = dns_queries_seen.load(Ordering::SeqCst);
    let pinned_proxy = http_proxy_for_resolved().unwrap().expect("wg socks5 proxy");
    let resolver = Arc::new(PinnedDnsResolver::new("wg-v4.invalid", addrs));
    let pinned_client = reqwest::Client::builder()
        .no_proxy()
        .proxy(pinned_proxy)
        .dns_resolver(resolver.clone())
        .use_preconfigured_tls(pinned_tls_test_config(pin))
        .build()
        .unwrap();
    let pinned_body = pinned_client
        .get(link_url)
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert_eq!(pinned_body, "wg-https-ok");
    assert_eq!(
        dns_queries_seen.load(Ordering::SeqCst),
        dns_before_fetch,
        "pinned WireGuard link-card request must not trigger a second DNS lookup"
    );
    // Strict resolver refuses any other host (no fallback to OS DNS).
    let wrong_host_err = pinned_client
        .get("https://other.invalid:8443/")
        .send()
        .await
        .unwrap_err();
    assert!(!wrong_host_err.to_string().is_empty());
    assert_eq!(dns_queries_seen.load(Ordering::SeqCst), dns_before_fetch);

    // 7. Concurrent `fail_active` + route switch tears down WireGuard without AB-BA deadlock
    // and wakes subscribers immediately.
    let mut sub = subscribe();
    let before_gen = sub.borrow_and_update().generation;
    let fail_thread = std::thread::spawn(move || {
        fail_active(before_gen, "simulated concurrent tunnel expiry");
    });
    reset_direct();
    fail_thread
        .join()
        .expect("fail_active thread did not deadlock");
    sub.changed().await.unwrap();
    assert!(sub.borrow().generation > before_gen);
    assert!(matches!(
        sub.borrow().status,
        RouteStatus::Active {
            kind: RouteKind::Direct,
            ..
        }
    ));

    let _ = stop_peer_tx.send(());
}

fn pinned_tls_test_config(pin: crate::auth::Fingerprint) -> rustls::ClientConfig {
    let provider = ring::default_provider();
    let verifier = Arc::new(TestPinVerifier {
        pin,
        algorithms: provider.signature_verification_algorithms,
    });
    rustls::ClientConfig::builder_with_provider(Arc::new(provider))
        .with_safe_default_protocol_versions()
        .unwrap()
        .dangerous()
        .with_custom_certificate_verifier(verifier)
        .with_no_client_auth()
}

#[derive(Debug)]
struct TestPinVerifier {
    pin: crate::auth::Fingerprint,
    algorithms: rustls::crypto::WebPkiSupportedAlgorithms,
}

impl rustls::client::danger::ServerCertVerifier for TestPinVerifier {
    fn verify_server_cert(
        &self,
        end_entity: &rustls::pki_types::CertificateDer<'_>,
        _: &[rustls::pki_types::CertificateDer<'_>],
        _: &rustls::pki_types::ServerName<'_>,
        _: &[u8],
        _: rustls::pki_types::UnixTime,
    ) -> Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
        if fingerprint(end_entity.as_ref()) != self.pin {
            return Err(rustls::Error::General("pin mismatch".into()));
        }
        Ok(rustls::client::danger::ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &rustls::pki_types::CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(message, cert, dss, &self.algorithms)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &rustls::pki_types::CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(message, cert, dss, &self.algorithms)
    }

    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        self.algorithms.supported_schemes()
    }
}

/// Minimal in-process WireGuard peer (`boringtun` + `smoltcp`) answering UDP DNS on
/// `10.77.0.1:53` and TLS HTTPS on `10.77.0.1:8443` and `[fd77::1]:8443`.
#[allow(clippy::too_many_arguments)]
fn spawn_controlled_wg_peer(
    udp: UdpSocket,
    peer_priv: StaticSecret,
    client_pub: PublicKey,
    psk: [u8; 32],
    tls_cfg: Arc<ServerConfig>,
    encrypted_packets: Arc<AtomicUsize>,
    dns_queries_seen: Arc<AtomicUsize>,
    mut stop_rx: oneshot::Receiver<()>,
) {
    struct PeerDevice {
        rx: std::collections::VecDeque<Vec<u8>>,
        tx: std::collections::VecDeque<Vec<u8>>,
    }
    struct RxTok(Vec<u8>);
    impl RxToken for RxTok {
        fn consume<R, F>(self, f: F) -> R
        where
            F: FnOnce(&[u8]) -> R,
        {
            f(&self.0)
        }
    }
    struct TxTok<'a>(&'a mut std::collections::VecDeque<Vec<u8>>);
    impl<'a> TxToken for TxTok<'a> {
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
    impl Device for PeerDevice {
        type RxToken<'a> = RxTok;
        type TxToken<'a> = TxTok<'a>;
        fn receive(&mut self, _: SmolInstant) -> Option<(Self::RxToken<'_>, Self::TxToken<'_>)> {
            let p = self.rx.pop_front()?;
            Some((RxTok(p), TxTok(&mut self.tx)))
        }
        fn transmit(&mut self, _: SmolInstant) -> Option<Self::TxToken<'_>> {
            Some(TxTok(&mut self.tx))
        }
        fn capabilities(&self) -> DeviceCapabilities {
            let mut caps = DeviceCapabilities::default();
            caps.medium = Medium::Ip;
            caps.max_transmission_unit = 1280;
            caps
        }
    }

    tokio::spawn(async move {
        let mut tunn = Tunn::new(peer_priv, client_pub, Some(psk), None, 1, None);
        let mut dev = PeerDevice {
            rx: Default::default(),
            tx: Default::default(),
        };
        let start = StdInstant::now();
        let now = |s: StdInstant| SmolInstant::from_millis(s.elapsed().as_millis() as i64);
        let mut iface = Interface::new(IfaceConfig::new(HardwareAddress::Ip), &mut dev, now(start));
        iface.update_ip_addrs(|addrs| {
            let _ = addrs.push(IpCidr::new(IpAddress::v4(10, 77, 0, 1), 24));
            let _ = addrs.push(IpCidr::new(
                IpAddress::Ipv6("fd77::1".parse::<Ipv6Addr>().unwrap()),
                64,
            ));
        });
        let mut sockets = SocketSet::new(vec![]);
        let dns_rx = udp::PacketBuffer::new(vec![udp::PacketMetadata::EMPTY; 16], vec![0u8; 4096]);
        let dns_tx = udp::PacketBuffer::new(vec![udp::PacketMetadata::EMPTY; 16], vec![0u8; 4096]);
        let mut dns_sock = udp::Socket::new(dns_rx, dns_tx);
        dns_sock.bind(53).unwrap();
        let dns_handle = sockets.add(dns_sock);

        // Pool of listening TCP sockets on port 8443 for IPv4 & IPv6 HTTPS connections.
        let mut tls_conns: Vec<(smoltcp::iface::SocketHandle, Option<ServerConnection>, bool)> = (0
            ..6)
            .map(|_| {
                let mut s = tcp::Socket::new(
                    tcp::SocketBuffer::new(vec![0u8; 32768]),
                    tcp::SocketBuffer::new(vec![0u8; 32768]),
                );
                s.listen(8443).unwrap();
                (sockets.add(s), None, false)
            })
            .collect();

        let mut wg_in = vec![0u8; 2048];
        let mut wg_out = vec![0u8; 2048];
        let mut peer_remote: Option<SocketAddr> = None;

        loop {
            iface.poll(now(start), &mut dev, &mut sockets);

            // Handle in-tunnel UDP DNS queries.
            {
                let ds = sockets.get_mut::<udp::Socket>(dns_handle);
                while let Ok((payload, meta)) = ds.recv() {
                    dns_queries_seen.fetch_add(1, Ordering::SeqCst);
                    if let Some(reply) = build_test_dns_reply(payload) {
                        let _ = ds.send_slice(&reply, meta.endpoint);
                    }
                }
            }

            // Handle in-tunnel pinned TLS HTTPS connections on port 8443.
            for (handle, tls_opt, replied) in &mut tls_conns {
                let sock = sockets.get_mut::<tcp::Socket>(*handle);
                if sock.state() == tcp::State::Closed {
                    *tls_opt = None;
                    *replied = false;
                    let _ = sock.listen(8443);
                    continue;
                }
                if sock.is_active() && tls_opt.is_none() {
                    *tls_opt = Some(ServerConnection::new(tls_cfg.clone()).unwrap());
                }
                if let Some(tls) = tls_opt.as_mut() {
                    if sock.can_recv() {
                        let _ = sock.recv(|buf| {
                            let mut slice: &[u8] = buf;
                            let _ = tls.read_tls(&mut slice);
                            let _ = tls.process_new_packets();
                            (buf.len(), ())
                        });
                    }
                    let mut plain = [0u8; 1024];
                    if !*replied
                        && let Ok(n) = std::io::Read::read(&mut tls.reader(), &mut plain)
                        && n > 0
                    {
                        let resp = b"HTTP/1.1 200 OK\r\nContent-Length: 11\r\nConnection: close\r\n\r\nwg-https-ok";
                        let _ = std::io::Write::write_all(&mut tls.writer(), resp);
                        tls.send_close_notify();
                        *replied = true;
                    }
                    let mut out = Vec::new();
                    while tls.wants_write() {
                        if tls.write_tls(&mut out).unwrap_or(0) == 0 {
                            break;
                        }
                    }
                    if !out.is_empty() && sock.can_send() {
                        let _ = sock.send_slice(&out);
                    }
                    if *replied && !tls.wants_write() && sock.may_send() {
                        sock.close();
                    }
                }
            }

            iface.poll(now(start), &mut dev, &mut sockets);

            if let Some(remote) = peer_remote {
                while let Some(pkt) = dev.tx.pop_front() {
                    if let TunnResult::WriteToNetwork(enc) = tunn.encapsulate(&pkt, &mut wg_out) {
                        encrypted_packets.fetch_add(1, Ordering::SeqCst);
                        let _ = udp.send_to(enc, remote).await;
                    }
                }
            }

            tokio::select! {
                _ = &mut stop_rx => break,
                _ = tokio::time::sleep(Duration::from_millis(5)) => {}
                recv = udp.recv_from(&mut wg_in) => {
                    let Ok((n, from)) = recv else { break };
                    peer_remote = Some(from);
                    encrypted_packets.fetch_add(1, Ordering::SeqCst);
                    let mut res = tunn.decapsulate(Some(from.ip()), &wg_in[..n], &mut wg_out);
                    loop {
                        match res {
                            TunnResult::WriteToNetwork(enc) => {
                                encrypted_packets.fetch_add(1, Ordering::SeqCst);
                                let _ = udp.send_to(enc, from).await;
                                res = tunn.decapsulate(None, &[], &mut wg_out);
                            }
                            TunnResult::WriteToTunnelV4(ip, _) | TunnResult::WriteToTunnelV6(ip, _) => {
                                dev.rx.push_back(ip.to_vec());
                                break;
                            }
                            _ => break,
                        }
                    }
                }
            }
        }
    });
}

/// Minimal RFC 1035 DNS responder answering `wg-v4.invalid` (A -> 10.77.0.1) and
/// `wg-v6.invalid` (AAAA -> fd77::1) inside the controlled WireGuard tunnel.
fn build_test_dns_reply(req: &[u8]) -> Option<Vec<u8>> {
    if req.len() < 12 {
        return None;
    }
    let mut pos = 12;
    let mut labels = Vec::new();
    while pos < req.len() {
        let len = usize::from(req[pos]);
        pos += 1;
        if len == 0 {
            break;
        }
        if pos + len > req.len() {
            return None;
        }
        labels.push(String::from_utf8_lossy(&req[pos..pos + len]).to_ascii_lowercase());
        pos += len;
    }
    if pos + 4 > req.len() {
        return None;
    }
    let qtype = u16::from_be_bytes([req[pos], req[pos + 1]]);
    let qend = pos + 4;
    let name = labels.join(".");

    let mut resp = Vec::new();
    resp.extend_from_slice(&req[0..2]); // ID
    let rdata: Option<&[u8]> = match (name.as_str(), qtype) {
        ("wg-v4.invalid", 1) => Some(&[10, 77, 0, 1]),
        ("wg-v6.invalid", 28) => {
            static V6: [u8; 16] = [0xfd, 0x77, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1];
            Some(&V6)
        }
        _ => None,
    };
    if let Some(rdata) = rdata {
        resp.extend_from_slice(&[0x81, 0x80]); // QR=1, RD=1, RA=1, NOERROR
        resp.extend_from_slice(&[0, 1, 0, 1, 0, 0, 0, 0]); // QD=1, AN=1
        resp.extend_from_slice(&req[12..qend]); // Question
        resp.extend_from_slice(&[0xC0, 0x0C]); // Name pointer to offset 12
        resp.extend_from_slice(&qtype.to_be_bytes());
        resp.extend_from_slice(&[0, 1]); // IN
        resp.extend_from_slice(&60u32.to_be_bytes()); // TTL
        resp.extend_from_slice(&(rdata.len() as u16).to_be_bytes());
        resp.extend_from_slice(rdata);
    } else {
        resp.extend_from_slice(&[0x81, 0x80]); // NOERROR, 0 answers (NODATA)
        resp.extend_from_slice(&[0, 1, 0, 0, 0, 0, 0, 0]);
        resp.extend_from_slice(&req[12..qend]);
    }
    Some(resp)
}
