//! Errors of the client.

use std::io;
use std::path::PathBuf;

use sixm_net::NetError;

/// Why a client could not connect or sync.
#[derive(Debug, thiserror::Error)]
pub enum ClientError {
    /// The network layer failed: the connection, the handshake or a download.
    #[error(transparent)]
    Net(#[from] NetError),
    /// A downloaded file could not be stored in the cache.
    #[error("cannot store {}: {source}", path.display())]
    Cache {
        /// Where the file was to be stored.
        path: PathBuf,
        /// The I/O error.
        #[source]
        source: io::Error,
    },
    /// The connection ended before the resource list arrived.
    #[error("the connection ended before the resource list: {0}")]
    Disconnected(String),
    /// The server described a file with a kind this client does not know.
    #[error("the server sent unknown file kind {0}")]
    UnknownKind(u8),
}
