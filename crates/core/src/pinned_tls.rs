use std::sync::Arc;
#[cfg(feature = "client")]
use std::time::Duration;

use rustls::{
    ClientConfig, DigitallySignedStruct, Error, SignatureScheme,
    client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier},
    crypto::{WebPkiSupportedAlgorithms, ring, verify_tls12_signature, verify_tls13_signature},
    pki_types::{CertificateDer, ServerName, UnixTime},
};
use sha2::{Digest, Sha256};
#[cfg(feature = "client")]
use tokio_tungstenite::{
    Connector, MaybeTlsStream, WebSocketStream, client_async_tls_with_config,
    tungstenite::client::IntoClientRequest,
};

pub use crate::auth::Fingerprint;

#[cfg(feature = "client")]
pub(crate) type Socket = WebSocketStream<MaybeTlsStream<crate::route::RouteStream>>;

#[cfg(feature = "livekit-tls")]
mod livekit;
#[cfg(feature = "livekit-tls")]
pub use livekit::register;

pub fn fingerprint(der: &[u8]) -> Fingerprint {
    Sha256::digest(der).into()
}

/// The server presented a TLS certificate other than the pinned one.
#[derive(Debug, thiserror::Error)]
#[error("server TLS certificate fingerprint mismatch")]
pub struct PinMismatch;

/// Whether an error, or any error it wraps, is a [`PinMismatch`] from the pinned verifier.
pub fn is_pin_mismatch(error: &(dyn std::error::Error + 'static)) -> bool {
    let mut next = Some(error);
    while let Some(error) = next {
        if error.is::<PinMismatch>() {
            return true;
        }
        if let Some(Error::Other(other)) = error.downcast_ref::<Error>() {
            return other.0.is::<PinMismatch>();
        }
        // An I/O error's source skips the error it wraps, so look inside it first.
        next = match error
            .downcast_ref::<std::io::Error>()
            .and_then(|io| io.get_ref())
        {
            Some(inner) => Some(inner as &(dyn std::error::Error + 'static)),
            None => error.source(),
        };
    }
    false
}

/// Build an HTTPS-only client using the configured trust roots or an explicit leaf certificate pin.
#[cfg(feature = "https-client")]
pub fn https_client(
    pin: Option<Fingerprint>,
) -> Result<reqwest::Client, Box<dyn std::error::Error + Send + Sync>> {
    build_https_client(pin.map(pinned_config).transpose()?)
}

/// Connect to the event stream using the same trust decision as the HTTPS API.
#[cfg(feature = "client")]
pub(crate) async fn events_socket(
    server: &crate::auth::KnownServer,
    token: &str,
) -> Result<Socket, Box<dyn std::error::Error + Send + Sync>> {
    let mut url = server.address.join("v1/events")?;
    url.set_scheme("wss").map_err(|_| "invalid event origin")?;
    let mut request = url.as_str().into_client_request()?;
    request
        .headers_mut()
        .insert("authorization", format!("Bearer {token}").parse()?);
    let connector = server
        .tls_fingerprint
        .map(pinned_config)
        .transpose()?
        .map(|config| Connector::Rustls(Arc::new(config)));
    let host = url.host_str().ok_or("event origin has no host")?.to_owned();
    let port = url
        .port_or_known_default()
        .ok_or("event origin has no port")?;
    // Room for a full chat record inline, so a later server can stop leaving out larger ones.
    let limits = tokio_tungstenite::tungstenite::protocol::WebSocketConfig::default()
        .max_message_size(Some(16_384))
        .max_frame_size(Some(16_384));
    let timeout = Duration::from_secs(10);
    let stream = crate::route::connect_tcp(&host, port, timeout).await?;
    let (socket, _) = tokio::time::timeout(
        timeout,
        client_async_tls_with_config(request, stream, Some(limits), connector),
    )
    .await??;
    Ok(socket)
}

fn pinned_config(pin: Fingerprint) -> Result<ClientConfig, rustls::Error> {
    let provider = ring::default_provider();
    let verifier = Arc::new(PinnedCertificate {
        pin,
        algorithms: provider.signature_verification_algorithms,
    });
    Ok(ClientConfig::builder_with_provider(Arc::new(provider))
        .with_safe_default_protocol_versions()?
        .dangerous()
        .with_custom_certificate_verifier(verifier)
        .with_no_client_auth())
}

fn build_https_client(
    tls: Option<ClientConfig>,
) -> Result<reqwest::Client, Box<dyn std::error::Error + Send + Sync>> {
    let mut builder = reqwest::Client::builder()
        .https_only(true)
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none());
    #[cfg(feature = "client")]
    {
        builder = crate::route::configure_reqwest(builder)?;
    }
    if let Some(tls) = tls {
        builder = builder.use_preconfigured_tls(tls);
    }
    Ok(builder.build()?)
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
            return Err(Error::Other(rustls::OtherError(Arc::new(PinMismatch))));
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
        let error = verifier
            .verify_server_cert(other.cert.der(), &[cert.cert.der().clone()], &ip, &[], now)
            .unwrap_err();
        assert!(is_pin_mismatch(&error));
    }

    /// A real handshake against another certificate reports the typed mismatch through
    /// reqwest's error chain.
    #[cfg(feature = "https-client")]
    #[tokio::test]
    async fn a_changed_certificate_is_reported_as_a_pin_mismatch() {
        use std::io::Read;

        let served = rcgen::generate_simple_self_signed(vec!["localhost".into()]).unwrap();
        let pinned = rcgen::generate_simple_self_signed(vec!["localhost".into()]).unwrap();
        let config = Arc::new(
            rustls::ServerConfig::builder_with_provider(Arc::new(ring::default_provider()))
                .with_safe_default_protocol_versions()
                .unwrap()
                .with_no_client_auth()
                .with_single_cert(
                    vec![served.cert.der().clone()],
                    rustls::pki_types::PrivateKeyDer::Pkcs8(
                        served.signing_key.serialize_der().into(),
                    ),
                )
                .unwrap(),
        );
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = std::thread::spawn(move || {
            let (socket, _) = listener.accept().unwrap();
            let connection = rustls::ServerConnection::new(config).unwrap();
            let mut stream = rustls::StreamOwned::new(connection, socket);
            let _ = stream.read(&mut [0; 1]);
        });
        let client = https_client(Some(fingerprint(pinned.cert.der().as_ref()))).unwrap();
        let error = client
            .get(format!("https://127.0.0.1:{port}/"))
            .send()
            .await
            .unwrap_err();
        assert!(is_pin_mismatch(&error), "{error:?}");
        server.join().unwrap();

        let unrelated = std::io::Error::other("connection refused");
        assert!(!is_pin_mismatch(&unrelated));
    }
}
