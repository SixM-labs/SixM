//! Constants and message types of the SixM wire protocol.

use std::time::Duration;

use serde::Serialize;

use crate::entities::EntityEntry;
use crate::files::{FileHash, ResourceEntry};
use crate::wire::{Args, StateValue};

/// Version of the wire protocol. A client must send it in its `Hello`.
pub const PROTOCOL_VERSION: u32 = 9;

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
/// Longest console line a client may send with [`ClientMessage::Command`], in bytes.
pub(crate) const MAX_COMMAND_LEN: usize = 4096;
/// Longest player name in characters, after trimming.
pub(crate) const MAX_PLAYER_NAME_LEN: usize = 64;
/// Longest convar name in bytes that travels to clients. A name is 1 byte or more.
pub const MAX_CONVAR_NAME_LEN: usize = 256;
/// Longest convar value in bytes that travels to clients.
pub const MAX_CONVAR_VALUE_LEN: usize = 4096;
/// Longest progress message of a deferral, in bytes. Longer messages are cut
/// on a character boundary when they are sent.
pub(crate) const MAX_DEFERRAL_MESSAGE_LEN: usize = 4096;

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

/// File requests a player may have queued before it is kicked.
pub(crate) const MAX_PENDING_REQUESTS: usize = 256;

/// File transfers served to one player at the same time. Further requests wait.
pub(crate) const MAX_CONCURRENT_TRANSFERS: usize = 8;

/// Reason sent to players that queued more file requests than allowed.
pub(crate) const TOO_MANY_REQUESTS: &str = "too many file requests";

/// Most entity transforms one client message may carry. A client sends more
/// in several messages.
pub(crate) const MAX_TRANSFORMS_PER_MESSAGE: usize = 1024;

/// Reason sent to every player and pending connection at shutdown.
pub(crate) const SHUTTING_DOWN: &str = "server shutting down";

/// Messages a client sends on the stream. Decoding is done by the wire module,
/// which must know every variant's tag.
#[derive(Debug, Serialize)]
pub(crate) enum ClientMessage {
    Hello {
        protocol: u32,
        name: String,
    },
    Event {
        name: String,
        args: Args,
    },
    /// Asks for the file with this hash. The server answers on a unidirectional
    /// stream, or with [`ServerMessage::FileUnavailable`].
    RequestFile {
        hash: FileHash,
    },
    /// A console line the server runs with the player's permissions. Lines
    /// longer than [`MAX_COMMAND_LEN`] bytes are rejected when decoded.
    Command {
        line: String,
    },
    /// New transforms of entities this client owns, as `(id, position,
    /// rotation, epoch)`, where `epoch` is the latest epoch the client knows
    /// for the entity. The server applies an update only when the player owns
    /// the entity, every value is finite and the epoch is the entity's current
    /// one. A message carries at most [`MAX_TRANSFORMS_PER_MESSAGE`] updates.
    EntityTransforms {
        updates: Vec<(u32, [f32; 3], [f32; 3], u32)>,
    },
}

impl ClientMessage {
    /// Variant index of [`ClientMessage::Hello`] on the wire.
    pub(crate) const HELLO_TAG: u32 = 0;
    /// Variant index of [`ClientMessage::Event`] on the wire.
    pub(crate) const EVENT_TAG: u32 = 1;
    /// Variant index of [`ClientMessage::RequestFile`] on the wire.
    pub(crate) const REQUEST_FILE_TAG: u32 = 2;
    /// Variant index of [`ClientMessage::Command`] on the wire.
    pub(crate) const COMMAND_TAG: u32 = 3;
    /// Variant index of [`ClientMessage::EntityTransforms`] on the wire.
    pub(crate) const ENTITY_TRANSFORMS_TAG: u32 = 4;
}

/// Messages the server sends on the stream. Decoding is done by the wire
/// module, which must know every variant's tag.
#[derive(Debug, Serialize)]
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
    /// Client files of the running resources, in start order. The snapshot
    /// sent on join is one or more frames with entries, which are additive
    /// (a resource may span frames), followed by a frame with no entries. An
    /// empty frame always ends the snapshot.
    Resources {
        entries: Vec<ResourceEntry>,
    },
    /// A resource started, with its client files. A client replaces any
    /// earlier entry of the same name.
    ResourceStarted {
        entry: ResourceEntry,
    },
    /// A resource stopped. Its files are no longer advertised.
    ResourceStopped {
        name: String,
    },
    /// The file requested with this hash is not advertised.
    FileUnavailable {
        hash: FileHash,
    },
    /// An entity was created. Clients add it to their copy, with its state bag.
    EntityCreated {
        entity: EntityEntry,
    },
    /// New positions and rotations, as `(id, position, rotation, epoch)`. The
    /// epoch changes each time the server moves the entity itself. Clients
    /// ignore an id they do not know. One frame may carry many updates.
    EntityTransforms {
        updates: Vec<(u32, [f32; 3], [f32; 3], u32)>,
    },
    /// An entity was deleted. Clients remove it, and its state bag follows as a
    /// removal.
    EntityDeleted {
        id: u32,
    },
    /// Entities that exist when a player joins, in ascending id order. A
    /// snapshot may span several frames, and each entry is a creation.
    EntitySnapshot {
        entities: Vec<EntityEntry>,
    },
    /// Replicated convars, as `(name, value)`. A nil `value` (`None`) means the
    /// convar is no longer replicated, so the client forgets it. A join snapshot
    /// may span several frames.
    Convars {
        entries: Vec<(String, Option<String>)>,
    },
    /// Progress text for a connection that is held by deferrals. The client
    /// keeps waiting for `Welcome` and treats the text as a log line.
    Deferral {
        message: String,
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
    /// Variant index of [`ServerMessage::Resources`] on the wire.
    pub(crate) const RESOURCES_TAG: u32 = 7;
    /// Variant index of [`ServerMessage::ResourceStarted`] on the wire.
    pub(crate) const RESOURCE_STARTED_TAG: u32 = 8;
    /// Variant index of [`ServerMessage::ResourceStopped`] on the wire.
    pub(crate) const RESOURCE_STOPPED_TAG: u32 = 9;
    /// Variant index of [`ServerMessage::FileUnavailable`] on the wire.
    pub(crate) const FILE_UNAVAILABLE_TAG: u32 = 10;
    /// Variant index of [`ServerMessage::EntityCreated`] on the wire.
    pub(crate) const ENTITY_CREATED_TAG: u32 = 11;
    /// Variant index of [`ServerMessage::EntityTransforms`] on the wire.
    pub(crate) const ENTITY_TRANSFORMS_TAG: u32 = 12;
    /// Variant index of [`ServerMessage::EntityDeleted`] on the wire.
    pub(crate) const ENTITY_DELETED_TAG: u32 = 13;
    /// Variant index of [`ServerMessage::EntitySnapshot`] on the wire.
    pub(crate) const ENTITY_SNAPSHOT_TAG: u32 = 14;
    /// Variant index of [`ServerMessage::Convars`] on the wire.
    pub(crate) const CONVARS_TAG: u32 = 15;
    /// Variant index of [`ServerMessage::Deferral`] on the wire.
    pub(crate) const DEFERRAL_TAG: u32 = 16;
}
