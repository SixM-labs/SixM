//! Constants and message types of the SixM wire protocol.

use std::time::Duration;

use serde::Serialize;

use crate::wire::{Args, StateValue};

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

/// Most values one message may decode into. Every value counts, map keys
/// included, so a frame cannot expand into more memory than this allows.
pub(crate) const MAX_DECODED_VALUES: usize = 65_536;

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

/// Messages a client sends on the stream. Decoding is done by the wire module,
/// which must know every variant's tag.
#[derive(Serialize)]
pub(crate) enum ClientMessage {
    Hello { protocol: u32, name: String },
    Event { name: String, args: Args },
}

impl ClientMessage {
    /// Variant index of [`ClientMessage::Hello`] on the wire.
    pub(crate) const HELLO_TAG: u32 = 0;
    /// Variant index of [`ClientMessage::Event`] on the wire.
    pub(crate) const EVENT_TAG: u32 = 1;
}

/// Messages the server sends on the stream. Decoding is done by the wire
/// module, which must know every variant's tag.
#[derive(Serialize)]
pub(crate) enum ServerMessage {
    Welcome {
        player: u32,
        server_name: String,
    },
    Rejected {
        reason: String,
    },
    Event {
        name: String,
        args: Args,
    },
    Kicked {
        reason: String,
    },
    /// A state bag key was set. A nil `value` means the key was deleted.
    StateBag {
        bag: String,
        key: String,
        value: StateValue,
    },
    /// A state bag was removed, because its player left.
    StateBagRemoved {
        bag: String,
    },
    /// Several state bag keys at once, in order, as `(bag, key, value)`. A
    /// nil `value` means the key was deleted. The client sees one
    /// `StateBag` event per entry.
    StateSnapshot {
        entries: Vec<(String, String, StateValue)>,
    },
}

impl ServerMessage {
    /// Variant index of [`ServerMessage::Welcome`] on the wire.
    pub(crate) const WELCOME_TAG: u32 = 0;
    /// Variant index of [`ServerMessage::Rejected`] on the wire.
    pub(crate) const REJECTED_TAG: u32 = 1;
    /// Variant index of [`ServerMessage::Event`] on the wire.
    pub(crate) const EVENT_TAG: u32 = 2;
    /// Variant index of [`ServerMessage::Kicked`] on the wire.
    pub(crate) const KICKED_TAG: u32 = 3;
    /// Variant index of [`ServerMessage::StateBag`] on the wire.
    pub(crate) const STATE_BAG_TAG: u32 = 4;
    /// Variant index of [`ServerMessage::StateBagRemoved`] on the wire.
    pub(crate) const STATE_BAG_REMOVED_TAG: u32 = 5;
    /// Variant index of [`ServerMessage::StateSnapshot`] on the wire.
    pub(crate) const STATE_SNAPSHOT_TAG: u32 = 6;
}
