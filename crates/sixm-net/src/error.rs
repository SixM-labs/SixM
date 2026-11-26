use std::io;
use std::net::SocketAddr;
use std::path::PathBuf;

use crate::identity::Fingerprint;

/// Errors returned by the network layer.
#[derive(Debug, thiserror::Error)]
pub enum NetError {
    /// An I/O operation failed.
    #[error("network I/O failed: {0}")]
    Io(#[from] io::Error),
    /// The UDP socket could not be bound.
    #[error("failed to bind {addr}: {source}")]
    Bind {
        addr: SocketAddr,
        #[source]
        source: io::Error,
    },
    /// The certificate or TLS configuration could not be built.
    #[error("TLS setup failed: {0}")]
    Tls(String),
    /// The persistent identity file could not be read, written or parsed.
    #[error("server identity file {}: {reason}", path.display())]
    Identity { path: PathBuf, reason: String },
    /// The server presented a certificate other than the pinned one.
    #[error("server certificate fingerprint {actual} does not match the pinned {expected}")]
    CertificateMismatch {
        expected: Fingerprint,
        actual: Fingerprint,
    },
    /// The network configuration is invalid.
    #[error("invalid configuration: {0}")]
    Config(String),
    /// The caller passed an argument the protocol cannot carry.
    #[error("invalid input: {0}")]
    InvalidInput(String),
    /// Connecting to the server failed.
    #[error("connection failed: {0}")]
    Connect(String),
    /// A connect or handshake step did not complete in time.
    #[error("timed out")]
    Timeout,
    /// The server refused the connection.
    #[error("rejected by server: {0}")]
    Rejected(String),
    /// The peer sent data the protocol does not allow.
    #[error("protocol violation: {0}")]
    Protocol(String),
    /// A value contains a function reference, which cannot leave its runtime.
    #[error("function references cannot be sent over the network")]
    FunctionNotSendable,
    /// A value is nested deeper than the protocol allows.
    #[error("value nested too deeply")]
    TooDeep,
    /// A frame is larger than the configured maximum.
    #[error("frame of {len} bytes exceeds the maximum of {max} bytes")]
    FrameTooLarge { len: usize, max: usize },
    /// The connection or the network thread is gone.
    #[error("connection closed")]
    Closed,
}
