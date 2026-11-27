//! Networking for SixM: a QUIC protocol between the server and its clients.
//!
//! The server runs all networking on its own thread and talks to the
//! single-threaded game loop only through [`NetServer`]'s non-blocking methods
//! and the [`NetEvent`] callback. [`NetClient`] is a blocking client for tests,
//! bots and the game client.
//!
//! Transport is QUIC with the `sixm/1` application protocol. The client opens
//! one bidirectional stream for ordinary messages, each frame
//! prefixed with its `u32` little-endian payload length. Resource files travel
//! on unidirectional streams of their own, so a large download never delays
//! ordinary events. Each latent event travels on a separate paced stream.

mod client;
mod connection;
mod entities;
mod error;
mod files;
#[doc(hidden)]
pub mod fuzzing;
mod identity;
mod latent;
mod protocol;
mod ratelimit;
mod server;
mod traffic;
mod transfer;
mod transport;
mod wire;

#[cfg(test)]
mod fuzz_tests;

pub use client::{ClientEvent, NetClient};
pub use entities::{EntityEntry, EntityTransform};
pub use error::NetError;
pub use files::{
    FILE_KIND_CLIENT, FILE_KIND_FILE, FILE_KIND_SHARED, FileEntry, FileHash, MAX_EXPORT_NAME_LEN,
    MAX_EXPORTS, MAX_FILE_SIZE, MAX_METADATA, MAX_METADATA_KEY_LEN, MAX_METADATA_VALUE_LEN,
    PublishedFile, PublishedResource, ResourceEntry, hash_hex,
};
pub use identity::{Fingerprint, Identity, ParseFingerprintError};
pub use protocol::{
    DEFAULT_LATENT_RATE, MAX_CONVAR_NAME_LEN, MAX_CONVAR_VALUE_LEN, MAX_LATENT_EVENT_LEN,
    MAX_LATENT_RATE, MIN_LATENT_RATE, PROTOCOL_VERSION,
};
pub use server::{ConnectionId, NetEvent, NetServer, ServerConfig};
pub use traffic::Traffic;
