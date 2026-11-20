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
use tokio::net::TcpStream;
#[cfg(feature = "client")]
use tokio_tungstenite::{
    Connector, MaybeTlsStream, WebSocketStream, connect_async_tls_with_config,
    tungstenite::client::IntoClientRequest,
};

pub use crate::auth::Fingerprint;

#[cfg(feature = "client")]
type Socket = WebSocketStream<MaybeTlsStream<TcpStream>>;

#[cfg(feature = "livekit-tls")]
mod livekit;
#[cfg(feature = "livekit-tls")]
pub use livekit::{PinnedElsewhere, register};

pub fn fingerprint(der: &[u8]) -> Fingerprint {
    Sha256::digest(der).into()
}

#[cfg(feature = "client")]
pub(crate) fn api_client(
    pin: Option<Fingerprint>,
) -> Result<reqwest::Client, Box<dyn std::error::Error + Send + Sync>> {
    https_client(pin.map(pinned_config).transpose()?)
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
    let limits = tokio_tungstenite::tungstenite::protocol::WebSocketConfig::default()
        .max_message_size(Some(4096))
        .max_frame_size(Some(4096));
    let (socket, _) = tokio::time::timeout(
        Duration::from_secs(10),
        connect_async_tls_with_config(request, Some(limits), false, connector),
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

fn https_client(
    tls: Option<ClientConfig>,
) -> Result<reqwest::Client, Box<dyn std::error::Error + Send + Sync>> {
    let mut builder = reqwest::Client::builder()
        .https_only(true)
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none());
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
}
