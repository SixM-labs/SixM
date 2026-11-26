//! TLS and QUIC configuration shared by the server and the client.

use std::sync::Arc;
use std::time::Duration;

use quinn::crypto::rustls::{QuicClientConfig, QuicServerConfig};
use quinn::{
    ClientConfig, Connection, ConnectionError, IdleTimeout, ServerConfig, TransportConfig, VarInt,
};
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::crypto::{CryptoProvider, verify_tls12_signature, verify_tls13_signature};
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer, ServerName, UnixTime};
use rustls::{DigitallySignedStruct, SignatureScheme};

use crate::error::NetError;
use crate::protocol::{ALPN, IDLE_TIMEOUT, KEEP_ALIVE_INTERVAL, SERVER_NAME};

/// Longest close reason sent with a QUIC `CONNECTION_CLOSE`, in bytes.
const MAX_CLOSE_REASON_LEN: usize = 512;

/// Time allowed for the peer to report how a connection ended.
pub(crate) const CLOSE_GRACE: Duration = Duration::from_secs(1);

/// Builds the server endpoint configuration with a fresh self-signed
/// certificate for `localhost`.
pub(crate) fn server_config() -> Result<ServerConfig, NetError> {
    let certified = rcgen::generate_simple_self_signed(vec![SERVER_NAME.to_owned()])
        .map_err(|err| NetError::Tls(err.to_string()))?;
    let cert = certified.cert.der().clone();
    let key = PrivateKeyDer::from(PrivatePkcs8KeyDer::from(
        certified.signing_key.serialize_der(),
    ));
    let mut tls = rustls::ServerConfig::builder_with_provider(provider())
        .with_protocol_versions(&[&rustls::version::TLS13])
        .map_err(|err| NetError::Tls(err.to_string()))?
        .with_no_client_auth()
        .with_single_cert(vec![cert], key)
        .map_err(|err| NetError::Tls(err.to_string()))?;
    tls.alpn_protocols = vec![ALPN.to_vec()];
    let quic = QuicServerConfig::try_from(tls).map_err(|err| NetError::Tls(err.to_string()))?;
    let mut config = ServerConfig::with_crypto(Arc::new(quic));
    config.transport_config(Arc::new(transport()?));
    Ok(config)
}

/// Builds the client configuration, which accepts any server certificate.
pub(crate) fn client_config() -> Result<ClientConfig, NetError> {
    let provider = provider();
    let mut tls = rustls::ClientConfig::builder_with_provider(Arc::clone(&provider))
        .with_protocol_versions(&[&rustls::version::TLS13])
        .map_err(|err| NetError::Tls(err.to_string()))?
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(AcceptAnyCertificate { provider }))
        .with_no_client_auth();
    tls.alpn_protocols = vec![ALPN.to_vec()];
    let quic = QuicClientConfig::try_from(tls).map_err(|err| NetError::Tls(err.to_string()))?;
    let mut config = ClientConfig::new(Arc::new(quic));
    config.transport_config(Arc::new(transport()?));
    Ok(config)
}

fn provider() -> Arc<CryptoProvider> {
    Arc::new(rustls::crypto::ring::default_provider())
}

fn transport() -> Result<TransportConfig, NetError> {
    let idle =
        IdleTimeout::try_from(IDLE_TIMEOUT).map_err(|err| NetError::Config(err.to_string()))?;
    let mut config = TransportConfig::default();
    config
        .max_idle_timeout(Some(idle))
        .keep_alive_interval(Some(KEEP_ALIVE_INTERVAL));
    Ok(config)
}

/// Certificate verifier that accepts any server certificate.
///
/// The handshake still checks that the server holds the private key of the
/// certificate it presents, so the traffic is encrypted for that key. It does
/// not authenticate the server: anyone can present their own certificate, so
/// a man in the middle is possible. Certificate pinning is planned.
#[derive(Debug)]
pub(crate) struct AcceptAnyCertificate {
    provider: Arc<CryptoProvider>,
}

impl ServerCertVerifier for AcceptAnyCertificate {
    fn verify_server_cert(
        &self,
        _end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        Ok(ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        verify_tls12_signature(
            message,
            cert,
            dss,
            &self.provider.signature_verification_algorithms,
        )
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        verify_tls13_signature(
            message,
            cert,
            dss,
            &self.provider.signature_verification_algorithms,
        )
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.provider
            .signature_verification_algorithms
            .supported_schemes()
    }
}

/// Describes how a connection ended, for use in disconnect reasons.
pub(crate) fn describe_close(error: &ConnectionError) -> String {
    match error {
        ConnectionError::ApplicationClosed(close) => {
            let reason = String::from_utf8_lossy(&close.reason).into_owned();
            if reason.is_empty() {
                "connection closed".to_owned()
            } else {
                reason
            }
        }
        other => other.to_string(),
    }
}

/// Waits briefly for the connection's close reason, falling back to a generic
/// description when none arrives.
pub(crate) async fn closing_reason(connection: &Connection) -> String {
    match tokio::time::timeout(CLOSE_GRACE, connection.closed()).await {
        Ok(error) => describe_close(&error),
        Err(_) => "stream closed".to_owned(),
    }
}

/// Closes a connection with an application code and a reason truncated to
/// [`MAX_CLOSE_REASON_LEN`] bytes on a character boundary.
pub(crate) fn close_connection(connection: &Connection, code: u32, reason: &str) {
    let mut end = reason.len().min(MAX_CLOSE_REASON_LEN);
    while !reason.is_char_boundary(end) {
        end -= 1;
    }
    connection.close(VarInt::from_u32(code), &reason.as_bytes()[..end]);
}
