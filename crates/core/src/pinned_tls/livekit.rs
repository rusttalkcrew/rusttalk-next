use std::time::Duration;
use std::{
    collections::HashMap,
    sync::{Arc, LazyLock, Mutex as SyncMutex},
};

use async_trait::async_trait;
use futures_util::{
    SinkExt, StreamExt,
    stream::{SplitSink, SplitStream},
};
use livekit_net::{
    Header, HttpClient, HttpMethod, HttpResponse, TransportError, WsClient, WsConnectResult,
    WsConnection,
};
use livekit_protocol::{SignalResponse, signal_response};
use prost::Message as _;
use rustls::ClientConfig;
use tokio::{net::TcpStream, sync::Mutex};
use tokio_tungstenite::{
    Connector, MaybeTlsStream, WebSocketStream, connect_async_tls_with_config,
    tungstenite::{Error as WsError, Message, client::IntoClientRequest},
};
use url::Url;

use super::{Fingerprint, build_https_client, pinned_config};

static TRANSPORT: LazyLock<Arc<PinnedTransport>> = LazyLock::new(Arc::default);

/// Register a trusted signaling origin with the SDK's process-wide transports.
pub fn register(
    server: &Url,
    pin: Option<Fingerprint>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let pin = pin.ok_or("missing server TLS certificate fingerprint")?;
    let origin = origin(server, "wss").ok_or("invalid pinned LiveKit signaling origin")?;
    let mut origins = TRANSPORT
        .origins
        .lock()
        .map_err(|_| "LiveKit transport lock poisoned")?;
    if let Some(existing) = origins.get(&origin) {
        return if existing.pin == pin {
            Ok(())
        } else {
            Err("LiveKit TLS certificate fingerprint changed at the known origin".into())
        };
    }

    let tls = pinned_config(pin)?;
    let http = build_https_client(Some(tls.clone()))?;
    if origins.is_empty() {
        if livekit_net::has_ws_client() || livekit_net::has_http_client() {
            return Err("the LiveKit transport has already been registered".into());
        }
        livekit_net::set_ws_client(TRANSPORT.clone());
        livekit_net::set_http_client(TRANSPORT.clone());
    }
    origins.insert(
        origin,
        LiveKitPin {
            pin,
            tls: Arc::new(tls),
            http,
        },
    );
    Ok(())
}

fn origin(url: &Url, scheme: &str) -> Option<(String, u16)> {
    if url.scheme() != scheme || !url.username().is_empty() || url.password().is_some() {
        return None;
    }
    Some((url.host_str()?.to_owned(), url.port_or_known_default()?))
}

#[derive(Default)]
struct PinnedTransport {
    origins: SyncMutex<HashMap<(String, u16), LiveKitPin>>,
}

#[derive(Clone)]
struct LiveKitPin {
    pin: Fingerprint,
    tls: Arc<ClientConfig>,
    http: reqwest::Client,
}

impl PinnedTransport {
    fn lookup(&self, url: &str, scheme: &str) -> Result<(Url, LiveKitPin), TransportError> {
        let url = Url::parse(url).map_err(connection_error)?;
        let origin = origin(&url, scheme)
            .ok_or_else(|| TransportError::Other("invalid LiveKit signaling origin".into()))?;
        let pin = self
            .origins
            .lock()
            .map_err(|_| TransportError::Other("LiveKit transport lock poisoned".into()))?
            .get(&origin)
            .cloned()
            .ok_or_else(|| TransportError::Other("unknown LiveKit signaling origin".into()))?;
        Ok((url, pin))
    }
}

#[async_trait]
impl WsClient for PinnedTransport {
    async fn connect(
        &self,
        url: String,
        headers: Vec<Header>,
        timeout_ms: u64,
    ) -> Result<WsConnectResult, TransportError> {
        let (url, pin) = self.lookup(&url, "wss")?;
        let mut request = url
            .as_str()
            .into_client_request()
            .map_err(connection_error)?;
        for header in headers {
            let name = header
                .name
                .parse::<reqwest::header::HeaderName>()
                .map_err(connection_error)?;
            let value = header.value.parse().map_err(connection_error)?;
            request.headers_mut().insert(name, value);
        }
        let connect =
            connect_async_tls_with_config(request, None, false, Some(Connector::Rustls(pin.tls)));
        let (socket, _) = tokio::time::timeout(Duration::from_millis(timeout_ms), connect)
            .await
            .map_err(|_| TransportError::Timeout)?
            .map_err(|error| match error {
                WsError::Http(response) => TransportError::Http {
                    status: response.status().as_u16(),
                },
                _ => connection_error(error),
            })?;
        let (writer, reader) = socket.split();
        Ok(WsConnectResult {
            connection: Arc::new(Connection {
                writer: Mutex::new(writer),
                reader: Mutex::new(reader),
            }),
        })
    }
}

#[async_trait]
impl HttpClient for PinnedTransport {
    async fn request(
        &self,
        method: HttpMethod,
        url: String,
        headers: Vec<Header>,
        body: Option<Vec<u8>>,
    ) -> Result<HttpResponse, TransportError> {
        let (url, pin) = self.lookup(&url, "https")?;
        let mut request = match method {
            HttpMethod::Get => pin.http.get(url),
            HttpMethod::Post => pin.http.post(url),
        };
        for header in headers {
            request = request.header(header.name, header.value);
        }
        if let Some(body) = body {
            request = request.body(body);
        }
        let response = request
            .send()
            .await
            .map_err(|error| connection_error(error.without_url()))?;
        // The SDK logs validation error bodies. Give it the status without server-controlled text.
        Ok(HttpResponse {
            status: response.status().as_u16(),
            headers: vec![],
            body: vec![],
        })
    }
}

type Socket = WebSocketStream<MaybeTlsStream<TcpStream>>;

struct Connection {
    writer: Mutex<SplitSink<Socket, Message>>,
    reader: Mutex<SplitStream<Socket>>,
}

#[async_trait]
impl WsConnection for Connection {
    async fn send(&self, frame: Vec<u8>) -> Result<(), TransportError> {
        self.writer
            .lock()
            .await
            .send(Message::Binary(frame.into()))
            .await
            .map_err(connection_error)
    }

    async fn recv(&self) -> Result<Option<Vec<u8>>, TransportError> {
        let mut reader = self.reader.lock().await;
        while let Some(message) = reader.next().await {
            match message.map_err(connection_error)? {
                Message::Binary(frame) => return without_ice_servers(&frame).map(Some),
                Message::Ping(_) => self
                    .writer
                    .lock()
                    .await
                    .flush()
                    .await
                    .map_err(connection_error)?,
                Message::Close(_) => {
                    let _ = self.writer.lock().await.flush().await;
                    return Ok(None);
                }
                _ => {}
            }
        }
        Ok(None)
    }

    async fn close(&self) {
        let _ = self.writer.lock().await.close().await;
    }
}

fn without_ice_servers(frame: &[u8]) -> Result<Vec<u8>, TransportError> {
    // An empty RoomOptions list tells the SDK to use the server's list, which can contain public STUN.
    let mut response = SignalResponse::decode(frame).map_err(connection_error)?;
    let ice_servers = match &mut response.message {
        Some(signal_response::Message::Join(join)) => &mut join.ice_servers,
        Some(signal_response::Message::Reconnect(reconnect)) => &mut reconnect.ice_servers,
        _ => return Ok(frame.to_vec()),
    };
    if ice_servers.is_empty() {
        return Ok(frame.to_vec());
    }
    ice_servers.clear();
    Ok(response.encode_to_vec())
}

fn connection_error(error: impl std::fmt::Display) -> TransportError {
    TransportError::Connection(error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn refuses_to_install_without_a_pin() {
        let server = Url::parse("wss://127.0.0.1:8443").unwrap();
        assert_eq!(
            register(&server, None).unwrap_err().to_string(),
            "missing server TLS certificate fingerprint"
        );
    }

    #[test]
    fn each_origin_uses_only_its_own_certificate() {
        use crate::pinned_tls::fingerprint;
        use rustls::{
            ClientConnection, ServerConfig, ServerConnection, crypto::ring,
            pki_types::PrivatePkcs8KeyDer,
        };

        let certs = [8443, 8444].map(|port| {
            let cert = rcgen::generate_simple_self_signed(vec!["localhost".into()]).unwrap();
            let pin = fingerprint(cert.cert.der());
            let url = Url::parse(&format!("wss://127.0.0.1:{port}")).unwrap();
            register(&url, Some(pin)).unwrap();
            let server = ServerConfig::builder_with_provider(Arc::new(ring::default_provider()))
                .with_safe_default_protocol_versions()
                .unwrap()
                .with_no_client_auth()
                .with_single_cert(
                    vec![cert.cert.der().clone()],
                    PrivatePkcs8KeyDer::from(cert.signing_key.serialize_der()).into(),
                )
                .unwrap();
            (url, pin, Arc::new(server))
        });
        for (i, (url, _, _)) in certs.iter().enumerate() {
            let https = url.as_str().replacen("wss:", "https:", 1);
            for (request, scheme) in [(url.as_str(), "wss"), (&https, "https")] {
                let (_, pin) = TRANSPORT.lookup(request, scheme).unwrap();
                for (j, (_, _, config)) in certs.iter().enumerate() {
                    let mut server = ServerConnection::new(config.clone()).unwrap();
                    let mut client =
                        ClientConnection::new(pin.tls.clone(), "127.0.0.1".try_into().unwrap())
                            .unwrap();

                    let mut flight = Vec::new();
                    client.write_tls(&mut flight).unwrap();
                    server.read_tls(&mut flight.as_slice()).unwrap();
                    server.process_new_packets().unwrap();
                    flight.clear();
                    server.write_tls(&mut flight).unwrap();
                    client.read_tls(&mut flight.as_slice()).unwrap();
                    assert_eq!(client.process_new_packets().is_ok(), i == j);
                    if i == j {
                        assert!(!client.is_handshaking());
                    }
                }
            }
        }
        assert!(register(&certs[0].0, Some(certs[1].1)).is_err());
        assert!(TRANSPORT.lookup("wss://127.0.0.1:8445/rtc", "wss").is_err());
        assert!(
            TRANSPORT
                .lookup("https://127.0.0.2:8443/rtc/validate", "https")
                .is_err()
        );
    }

    #[test]
    fn removes_ice_servers_on_join_and_reconnect() {
        use livekit_protocol::{IceServer, JoinResponse, ReconnectResponse};
        use signal_response::Message::{Join, Reconnect};

        let advertised = vec![IceServer {
            urls: vec!["stun:example.com:3478".into()],
            ..Default::default()
        }];
        let join = JoinResponse {
            ice_servers: advertised.clone(),
            subscriber_primary: true,
            ..Default::default()
        };
        let reconnect = ReconnectResponse {
            ice_servers: advertised,
            ..Default::default()
        };
        let filtered = |message| {
            let frame = SignalResponse {
                message: Some(message),
            }
            .encode_to_vec();
            SignalResponse::decode(without_ice_servers(&frame).unwrap().as_slice())
                .unwrap()
                .message
        };
        assert_eq!(
            filtered(Join(join)),
            Some(Join(JoinResponse {
                subscriber_primary: true,
                ..Default::default()
            }))
        );
        assert_eq!(
            filtered(Reconnect(reconnect)),
            Some(Reconnect(ReconnectResponse::default()))
        );
    }
}
