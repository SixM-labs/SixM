//! Constants and message types of the SixM wire protocol.

use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::wire::Args;

/// Version of the wire protocol. A client must send it in its `Hello`.
pub const PROTOCOL_VERSION: u32 = 1;

/// Application protocol identifier negotiated during the TLS handshake.
pub(crate) const ALPN: &[u8] = b"sixm/1";

/// Server name used for the TLS handshake. Clients accept any certificate, so
/// the name only fills the SNI field.
pub(crate) const SERVER_NAME: &str = "localhost";

/// Largest frame payload accepted, in bytes. Also the limit for server frames
/// read by clients, so a server may not configure a larger maximum.
pub(crate) const MAX_FRAME: usize = 1024 * 1024;

/// Deepest nesting of container values that is encoded or decoded. A value
/// directly inside an event's argument list has depth 1.
pub(crate) const MAX_VALUE_DEPTH: usize = 64;

/// Longest event name in bytes.
pub(crate) const MAX_EVENT_NAME_LEN: usize = 256;

/// Longest player name in characters, after trimming.
pub(crate) const MAX_PLAYER_NAME_LEN: usize = 64;

/// Time a connection may stay silent before QUIC closes it.
pub(crate) const IDLE_TIMEOUT: Duration = Duration::from_secs(30);

/// Interval of QUIC keep-alive packets.
pub(crate) const KEEP_ALIVE_INTERVAL: Duration = Duration::from_secs(5);

/// Frames queued for one player before the player is kicked.
pub(crate) const OUTBOUND_QUEUE_LEN: usize = 1024;

/// Reason sent to players whose outbound queue overflowed.
pub(crate) const SEND_QUEUE_OVERFLOW: &str = "send queue overflow";

/// Reason sent to players that exceeded the flood threshold.
pub(crate) const EVENT_FLOOD: &str = "event flood";

/// Reason sent to every player and pending connection at shutdown.
pub(crate) const SHUTTING_DOWN: &str = "server shutting down";

/// First message a client sends on the stream.
#[derive(Serialize, Deserialize)]
pub(crate) enum ClientMessage {
    Hello { protocol: u32, name: String },
    Event { name: String, args: Args },
}

impl ClientMessage {
    /// Variant index of [`ClientMessage::Event`] on the wire.
    pub(crate) const EVENT_TAG: u32 = 1;
}

/// Message the server sends on the stream.
#[derive(Serialize, Deserialize)]
pub(crate) enum ServerMessage {
    Welcome { player: u32, server_name: String },
    Rejected { reason: String },
    Event { name: String, args: Args },
    Kicked { reason: String },
}

impl ServerMessage {
    /// Variant index of [`ServerMessage::Event`] on the wire.
    pub(crate) const EVENT_TAG: u32 = 2;
}
