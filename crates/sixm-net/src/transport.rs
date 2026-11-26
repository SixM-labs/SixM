//! TLS and QUIC configuration shared by the server and the client.

use std::sync::{Arc, OnceLock};
use std::time::Duration;

use quinn::crypto::rustls::{QuicClientConfig, QuicServerConfig};
use quinn::{
    ClientConfig, Connection, ConnectionError, IdleTimeout, ServerConfig, TransportConfig, VarInt,
};
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::crypto::{CryptoProvider, verify_tls12_signature, verify_tls13_signature};
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use rustls::{DigitallySignedStruct, SignatureScheme};

use crate::error::NetError;
use crate::identity::{Credentials, Fingerprint};
use crate::protocol::{ALPN, IDLE_TIMEOUT, KEEP_ALIVE_INTERVAL};

/// Longest close reason sent with a QUIC `CONNECTION_CLOSE`, in bytes.
const MAX_CLOSE_REASON_LEN: usize = 512;

/// Time allowed for the peer to report how a connection ended.
pub(crate) const CLOSE_GRACE: Duration = Duration::from_secs(1);

/// Builds the server endpoint configuration for `credentials`.
pub(crate) fn server_config(credentials: Credentials) -> Result<ServerConfig, NetError> {
    let (certificate, key) = credentials.into_parts();
    let mut tls = rustls::ServerConfig::builder_with_provider(provider())
        .with_protocol_versions(&[&rustls::version::TLS13])
        .map_err(|err| NetError::Tls(err.to_string()))?
        .with_no_client_auth()
        .with_single_cert(vec![certificate], key)
        .map_err(|err| NetError::Tls(err.to_string()))?;
    tls.alpn_protocols = vec![ALPN.to_vec()];
    let quic = QuicServerConfig::try_from(tls).map_err(|err| NetError::Tls(err.to_string()))?;
    let mut config = ServerConfig::with_crypto(Arc::new(quic));
    config.transport_config(Arc::new(transport()?));
    Ok(config)
}

/// Builds the client configuration that checks server certificates with
/// `verifier`.
pub(crate) fn client_config(verifier: &Arc<ServerVerifier>) -> Result<ClientConfig, NetError> {
    let certificate_verifier: Arc<dyn ServerCertVerifier> = verifier.clone();
    let mut tls = rustls::ClientConfig::builder_with_provider(Arc::clone(&verifier.provider))
        .with_protocol_versions(&[&rustls::version::TLS13])
        .map_err(|err| NetError::Tls(err.to_string()))?
        .dangerous()
        .with_custom_certificate_verifier(certificate_verifier)
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

/// How a client decides whether to trust the server's certificate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Trust {
    /// Accept any certificate. The handshake still proves that the server
    /// holds the key of the certificate it presents, but nothing ties that
    /// certificate to a known server, so a man in the middle is possible.
    Any,
    /// Accept only the certificate with this fingerprint.
    Pinned(Fingerprint),
}

/// Certificate verifier for the client. Handshake signatures are always
/// checked with the ring provider; the certificate itself is judged by the
/// [`Trust`] mode.
#[derive(Debug)]
pub(crate) struct ServerVerifier {
    provider: Arc<CryptoProvider>,
    trust: Trust,
    /// The certificate that failed a pin, kept so the caller can report both
    /// fingerprints.
    mismatch: OnceLock<Fingerprint>,
}

impl ServerVerifier {
    pub(crate) fn new(trust: Trust) -> Arc<Self> {
        Arc::new(Self {
            provider: provider(),
            trust,
            mismatch: OnceLock::new(),
        })
    }

    /// The error for a failed handshake. A certificate that failed the pin
    /// is reported as a mismatch, anything else as a connection failure.
    pub(crate) fn handshake_error(&self, err: &ConnectionError) -> NetError {
        match (self.trust, self.mismatch.get()) {
            (Trust::Pinned(expected), Some(&actual)) => {
                NetError::CertificateMismatch { expected, actual }
            }
            _ => NetError::Connect(err.to_string()),
        }
    }
}

impl ServerCertVerifier for ServerVerifier {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        match self.trust {
            Trust::Any => Ok(ServerCertVerified::assertion()),
            Trust::Pinned(expected) => {
                let actual = Fingerprint::of(end_entity);
                if actual == expected {
                    Ok(ServerCertVerified::assertion())
                } else {
                    let _ = self.mismatch.set(actual);
                    Err(rustls::Error::General(
                        "server certificate does not match the pinned fingerprint".to_owned(),
                    ))
                }
            }
        }
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
