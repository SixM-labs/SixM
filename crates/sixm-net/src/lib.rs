//! Networking for SixM: a QUIC protocol between the server and its clients.
//!
//! The server runs all networking on its own thread and talks to the
//! single-threaded game loop only through [`NetServer`]'s non-blocking methods
//! and the [`NetEvent`] callback. [`NetClient`] is a blocking client for tests,
//! bots and the game client.
//!
//! Transport is QUIC with the `sixm/1` application protocol. The client opens
//! one bidirectional stream and every message flows on it in order, each frame
//! prefixed with its `u32` little-endian payload length. Resource files travel
//! on unidirectional streams of their own, so a large download never delays
//! events: the server opens one for each file the client requests.

mod client;
mod connection;
mod error;
mod files;
mod identity;
mod protocol;
mod ratelimit;
mod server;
mod transfer;
mod transport;
mod wire;

pub use client::{ClientEvent, NetClient};
pub use error::NetError;
pub use files::{
    FILE_KIND_CLIENT, FILE_KIND_FILE, FILE_KIND_SHARED, FileEntry, FileHash, MAX_EXPORT_NAME_LEN,
    MAX_EXPORTS, MAX_FILE_SIZE, PublishedFile, PublishedResource, ResourceEntry, hash_hex,
};
pub use identity::{Fingerprint, Identity, ParseFingerprintError};
pub use protocol::PROTOCOL_VERSION;
pub use server::{ConnectionId, NetEvent, NetServer, ServerConfig};
