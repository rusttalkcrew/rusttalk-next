use std::{
    sync::{Arc, OnceLock},
    time::Duration,
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
use rustls::{
    ClientConfig, DigitallySignedStruct, Error, SignatureScheme,
    client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier},
    crypto::{WebPkiSupportedAlgorithms, ring, verify_tls12_signature, verify_tls13_signature},
    pki_types::{CertificateDer, ServerName, UnixTime},
};
use sha2::{Digest, Sha256};
use tokio::{net::TcpStream, sync::Mutex};
use tokio_tungstenite::{
    Connector, MaybeTlsStream, WebSocketStream, connect_async_tls_with_config,
    tungstenite::{Error as WsError, Message, client::IntoClientRequest},
};
use url::Url;

pub type Fingerprint = [u8; 32];

pub fn fingerprint(der: &[u8]) -> Fingerprint {
    Sha256::digest(der).into()
}

/// Install the SDK's process-wide transports for one self-hosted server at startup.
// shortcut: one pinned server per process, upgrade to a host-to-pin map for the multi-server client (W15).
pub fn register(
    server: &Url,
    pin: Option<Fingerprint>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let pin = pin.ok_or("missing server TLS certificate fingerprint")?;
    if server.scheme() != "wss" || server.host().is_none() {
        return Err("the pinned LiveKit server must use wss".into());
    }

    let provider = ring::default_provider();
    let verifier = Arc::new(PinnedCertificate {
        pin,
        algorithms: provider.signature_verification_algorithms,
    });
    let tls = ClientConfig::builder_with_provider(Arc::new(provider))
        .with_safe_default_protocol_versions()?
        .dangerous()
        .with_custom_certificate_verifier(verifier)
        .with_no_client_auth();
    let http = reqwest::Client::builder()
        .use_preconfigured_tls(tls.clone())
        .https_only(true)
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .build()?;
    let transport = Arc::new(PinnedTransport {
        server: server.clone(),
        tls: Arc::new(tls),
        http,
    });

    static REGISTERED: OnceLock<()> = OnceLock::new();
    if REGISTERED.set(()).is_err() || livekit_net::has_ws_client() || livekit_net::has_http_client()
    {
        return Err("the LiveKit transport has already been registered".into());
    }
    livekit_net::set_ws_client(transport.clone());
    livekit_net::set_http_client(transport);
    Ok(())
}

#[derive(Debug)]
struct PinnedCertificate {
    pin: Fingerprint,
    algorithms: WebPkiSupportedAlgorithms,
}

impl ServerCertVerifier for PinnedCertificate {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        _: &[CertificateDer<'_>],
        _: &ServerName<'_>,
        _: &[u8],
        _: UnixTime,
    ) -> Result<ServerCertVerified, Error> {
        if fingerprint(end_entity.as_ref()) != self.pin {
            return Err(Error::General(
                "server TLS certificate fingerprint mismatch".into(),
            ));
        }
        Ok(ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        signature: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, Error> {
        verify_tls12_signature(message, cert, signature, &self.algorithms)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        signature: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, Error> {
        verify_tls13_signature(message, cert, signature, &self.algorithms)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.algorithms.supported_schemes()
    }
}

struct PinnedTransport {
    server: Url,
    tls: Arc<ClientConfig>,
    http: reqwest::Client,
}

impl PinnedTransport {
    fn server_url(&self, url: &str, scheme: &str) -> Result<Url, TransportError> {
        let url = Url::parse(url).map_err(connection_error)?;
        if url.scheme() != scheme
            || url.host() != self.server.host()
            || url.port_or_known_default() != self.server.port_or_known_default()
            || !url.username().is_empty()
            || url.password().is_some()
        {
            return Err(TransportError::Other(
                "unexpected LiveKit signaling origin".into(),
            ));
        }
        Ok(url)
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
        let url = self.server_url(&url, "wss")?;
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
        let connect = connect_async_tls_with_config(
            request,
            None,
            false,
            Some(Connector::Rustls(self.tls.clone())),
        );
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
        let url = self.server_url(&url, "https")?;
        let mut request = match method {
            HttpMethod::Get => self.http.get(url),
            HttpMethod::Post => self.http.post(url),
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
    fn accepts_only_the_pinned_leaf_even_for_another_hostname() {
        let cert = rcgen::generate_simple_self_signed(vec!["localhost".into()]).unwrap();
        let other = rcgen::generate_simple_self_signed(vec!["localhost".into()]).unwrap();
        let verifier = PinnedCertificate {
            pin: fingerprint(cert.cert.der().as_ref()),
            algorithms: ring::default_provider().signature_verification_algorithms,
        };
        let ip = "192.0.2.1".try_into().unwrap();
        let now = UnixTime::now();
        assert!(
            verifier
                .verify_server_cert(cert.cert.der(), &[], &ip, &[], now)
                .is_ok()
        );
        assert!(
            verifier
                .verify_server_cert(other.cert.der(), &[cert.cert.der().clone()], &ip, &[], now)
                .is_err()
        );
    }

    #[test]
    fn refuses_to_install_without_a_pin() {
        let server = Url::parse("wss://127.0.0.1:8443").unwrap();
        assert_eq!(
            register(&server, None).unwrap_err().to_string(),
            "missing server TLS certificate fingerprint"
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
