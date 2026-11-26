//! Binary encoding of script values and frames.
//!
//! A frame is a `u32` little-endian payload length followed by a postcard
//! payload. Values are encoded as postcard enums, strings as raw bytes and
//! arrays and maps as sequences. Function references have no encoding and
//! cannot be decoded, so a peer can never forge a reference into server code.
//!
//! Decoding counts every value against [`MAX_DECODED_VALUES`] while it runs,
//! so a small frame cannot make the decoder allocate more than that many values.

use std::cell::Cell;
use std::fmt;
use std::sync::Arc;

use quinn::{ReadExactError, RecvStream};
use serde::de::{
    self, DeserializeSeed, Deserializer, EnumAccess, SeqAccess, VariantAccess, Visitor,
};
use serde::ser::{SerializeSeq, SerializeStructVariant};
use serde::{Serialize, Serializer};
use sixm_core::{MAX_STATE_NAME_LEN, Value};
use tracing::warn;

use crate::error::NetError;
use crate::files::{
    FILE_KIND_FILE, FileEntry, FileHash, MAX_FILE_NAME_LEN, MAX_FILE_SIZE, MAX_RESOURCE_NAME_LEN,
    ResourceEntry,
};
use crate::protocol::{
    ClientMessage, MAX_DECODED_VALUES, MAX_EVENT_NAME_LEN, MAX_VALUE_DEPTH, ServerMessage,
};

const FRAME_HEADER_LEN: usize = 4;
const MESSAGE_NAME: &str = "Message";
const VALUE_NAME: &str = "Value";
const TAG_NIL: u32 = 0;
const TAG_BOOL: u32 = 1;
const TAG_INT: u32 = 2;
const TAG_FLOAT: u32 = 3;
const TAG_STRING: u32 = 4;
const TAG_ARRAY: u32 = 5;
const TAG_MAP: u32 = 6;
const VALUE_VARIANTS: &[&str] = &["Nil", "Bool", "Int", "Float", "String", "Array", "Map"];
const CLIENT_VARIANTS: &[&str] = &["Hello", "Event", "RequestFile"];
const SERVER_VARIANTS: &[&str] = &[
    "Welcome",
    "Rejected",
    "Event",
    "Kicked",
    "StateBag",
    "StateBagRemoved",
    "StateSnapshot",
    "Resources",
    "ResourceStarted",
    "ResourceStopped",
    "FileUnavailable",
];
/// Most files one snapshot entry carries. Larger resources are split into
/// several entries of the same name, so one entry always fits a frame.
const MAX_FILES_PER_ENTRY: usize = 256;

/// Argument list of an event. Decoding enforces [`MAX_VALUE_DEPTH`] and
/// [`MAX_DECODED_VALUES`].
#[derive(Debug, Clone, PartialEq, Default)]
pub(crate) struct Args(pub(crate) Vec<Value>);

impl Serialize for Args {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        Elements(&self.0).serialize(serializer)
    }
}

/// Borrowed encoding of an event, so that broadcasting never copies the
/// arguments. `tag` must be the event variant index of the receiving message.
pub(crate) struct EventFrame<'a> {
    pub(crate) tag: u32,
    pub(crate) name: &'a str,
    pub(crate) args: &'a [Value],
}

impl Serialize for EventFrame<'_> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut variant =
            serializer.serialize_struct_variant(MESSAGE_NAME, self.tag, "Event", 2)?;
        variant.serialize_field("name", self.name)?;
        variant.serialize_field("args", &Elements(self.args))?;
        variant.end()
    }
}

/// A state value as the server sends it. Nil means the key was deleted.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct StateValue(pub(crate) Value);

impl Serialize for StateValue {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        Encode(&self.0).serialize(serializer)
    }
}

/// Borrowed encoding of a state change, so that broadcasting never copies the value.
struct StateFrame<'a> {
    bag: &'a str,
    key: &'a str,
    value: &'a Value,
}

impl Serialize for StateFrame<'_> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut variant = serializer.serialize_struct_variant(
            MESSAGE_NAME,
            ServerMessage::STATE_BAG_TAG,
            "StateBag",
            3,
        )?;
        variant.serialize_field("bag", self.bag)?;
        variant.serialize_field("key", self.key)?;
        variant.serialize_field("value", &Encode(self.value))?;
        variant.end()
    }
}

/// Checks that a value list can be encoded: no function references and no
/// nesting beyond [`MAX_VALUE_DEPTH`].
pub(crate) fn check_values(values: &[Value]) -> Result<(), NetError> {
    values.iter().try_for_each(|value| check_value(value, 1))
}

fn check_value(value: &Value, depth: usize) -> Result<(), NetError> {
    if depth > MAX_VALUE_DEPTH {
        return Err(NetError::TooDeep);
    }
    match value {
        Value::FuncRef(_) => Err(NetError::FunctionNotSendable),
        Value::Array(items) => items
            .iter()
            .try_for_each(|item| check_value(item, depth + 1)),
        Value::Map(entries) => entries.iter().try_for_each(|(key, value)| {
            check_value(key, depth + 1)?;
            check_value(value, depth + 1)
        }),
        Value::Nil | Value::Bool(_) | Value::Int(_) | Value::Float(_) | Value::String(_) => Ok(()),
    }
}

/// Validates an event name, which must be 1 to 256 bytes.
pub(crate) fn validate_event_name(name: &str) -> Result<(), NetError> {
    validate_length("event name", name, MAX_EVENT_NAME_LEN)
}

/// Checks that `name` is 1 to `max` bytes long. `what` names it in the error.
pub(crate) fn validate_length(what: &str, name: &str, max: usize) -> Result<(), NetError> {
    if name.is_empty() || name.len() > max {
        return Err(NetError::InvalidInput(format!(
            "{what} must be 1 to {max} bytes, got {}",
            name.len()
        )));
    }
    Ok(())
}

/// Encodes a state change as a complete frame, validating the names and the value first.
pub(crate) fn encode_state(
    bag: &str,
    key: &str,
    value: &Value,
    max_frame: usize,
) -> Result<Arc<[u8]>, NetError> {
    validate_length("state bag name", bag, MAX_STATE_NAME_LEN)?;
    validate_length("state key", key, MAX_STATE_NAME_LEN)?;
    check_values(std::slice::from_ref(value))?;
    encode_frame(&StateFrame { bag, key, value }, max_frame)
}

/// Encodes the removal of a state bag as a complete frame.
pub(crate) fn encode_state_removed(bag: &str, max_frame: usize) -> Result<Arc<[u8]>, NetError> {
    validate_length("state bag name", bag, MAX_STATE_NAME_LEN)?;
    encode_frame(
        &ServerMessage::StateBagRemoved {
            bag: bag.to_owned(),
        },
        max_frame,
    )
}

/// Encodes a snapshot of state changes as the fewest frames that can carry
/// them in order. Each frame holds whole entries, and stays within `max_frame`
/// bytes and [`MAX_DECODED_VALUES`] decoded values, so a receiver can always
/// decode it. An entry too large for any frame on its own is skipped with a
/// warning. Names and values are validated up front, so an invalid entry fails
/// the whole call.
pub(crate) fn encode_state_snapshot(
    entries: &[(&str, &str, &Value)],
    max_frame: usize,
) -> Result<Vec<Arc<[u8]>>, NetError> {
    let mut frames = Vec::new();
    let mut chunk = SnapshotChunk::new(ServerMessage::STATE_SNAPSHOT_TAG);
    for &(bag, key, value) in entries {
        validate_length("state bag name", bag, MAX_STATE_NAME_LEN)?;
        validate_length("state key", key, MAX_STATE_NAME_LEN)?;
        check_values(std::slice::from_ref(value))?;
        let encoded = postcard::to_extend(&(bag, key, Encode(value)), Vec::<u8>::new())
            .map_err(|err| NetError::Protocol(format!("cannot encode message: {err}")))?;
        let values = count_values(value);
        if !chunk.is_empty() && !chunk.accepts(encoded.len(), values, max_frame) {
            frames.push(chunk.seal(max_frame)?);
            chunk = SnapshotChunk::new(ServerMessage::STATE_SNAPSHOT_TAG);
        }
        if !chunk.accepts(encoded.len(), values, max_frame) {
            warn!(
                bag,
                key, "state entry is too large for one frame, skipping it"
            );
            continue;
        }
        chunk.push(&encoded, values);
    }
    if !chunk.is_empty() {
        frames.push(chunk.seal(max_frame)?);
    }
    Ok(frames)
}

/// The part of a [`ResourceEntry`] that one snapshot chunk carries: a name and
/// a run of its files. Its encoding is the encoding of a `ResourceEntry`.
#[derive(Serialize)]
struct EntryPart<'a> {
    name: &'a str,
    files: &'a [FileEntry],
}

/// Encodes the client files of `entries` as the frames of a `Resources`
/// snapshot, in order, followed by an empty frame that ends it. A resource with
/// more than [`MAX_FILES_PER_ENTRY`] files is split across consecutive entries
/// of the same name. Every frame stays within `max_frame` bytes and the
/// decoding budget.
pub(crate) fn encode_resources_snapshot(
    entries: &[ResourceEntry],
    max_frame: usize,
) -> Result<Vec<Arc<[u8]>>, NetError> {
    let mut frames = Vec::new();
    let mut chunk = SnapshotChunk::new(ServerMessage::RESOURCES_TAG);
    for entry in entries {
        // A resource without files still needs one part, so that it is listed.
        let parts: Vec<&[FileEntry]> = if entry.files.is_empty() {
            vec![&entry.files[..]]
        } else {
            entry.files.chunks(MAX_FILES_PER_ENTRY).collect()
        };
        for files in parts {
            let part = EntryPart {
                name: &entry.name,
                files,
            };
            let encoded = postcard::to_extend(&part, Vec::<u8>::new())
                .map_err(|err| NetError::Protocol(format!("cannot encode message: {err}")))?;
            // The entry itself and each file count as one decoded value.
            let units = 1 + files.len();
            if !chunk.is_empty() && !chunk.accepts(encoded.len(), units, max_frame) {
                frames.push(chunk.seal(max_frame)?);
                chunk = SnapshotChunk::new(ServerMessage::RESOURCES_TAG);
            }
            chunk.push(&encoded, units);
        }
    }
    if !chunk.is_empty() {
        frames.push(chunk.seal(max_frame)?);
    }
    frames.push(encode_frame(
        &ServerMessage::Resources {
            entries: Vec::new(),
        },
        max_frame,
    )?);
    Ok(frames)
}

/// Entries of a snapshot frame that are being collected. `bytes` holds the
/// encoded entries back to back, and `values` counts their decoded values.
/// `tag` is the variant index of the frame's message.
struct SnapshotChunk {
    tag: u32,
    bytes: Vec<u8>,
    count: usize,
    values: usize,
}

impl SnapshotChunk {
    fn new(tag: u32) -> Self {
        Self {
            tag,
            bytes: Vec::new(),
            count: 0,
            values: 0,
        }
    }

    fn is_empty(&self) -> bool {
        self.count == 0
    }

    /// Whether an entry of `entry_len` bytes and `values` decoded values can
    /// join this chunk without exceeding the frame or the decoding budget.
    fn accepts(&self, entry_len: usize, values: usize, max_frame: usize) -> bool {
        let payload = varint_len(u64::from(self.tag))
            + varint_len(u64::try_from(self.count + 1).unwrap_or(u64::MAX))
            + self.bytes.len()
            + entry_len;
        FRAME_HEADER_LEN + payload <= max_frame && self.values + values <= MAX_DECODED_VALUES
    }

    fn push(&mut self, encoded: &[u8], values: usize) {
        self.bytes.extend_from_slice(encoded);
        self.count += 1;
        self.values += values;
    }

    /// Writes the variant index and entry count, then the entries, as one frame.
    /// A variant index and a sequence length are both varints, so the first two
    /// fields form the start of a snapshot message with a sequence payload.
    fn seal(self, max_frame: usize) -> Result<Arc<[u8]>, NetError> {
        let mut buffer = postcard::to_extend(&(self.tag, self.count), vec![0_u8; FRAME_HEADER_LEN])
            .map_err(|err| NetError::Protocol(format!("cannot encode message: {err}")))?;
        buffer.extend_from_slice(&self.bytes);
        finish_frame(buffer, max_frame)
    }
}

/// Number of bytes that postcard uses to encode `n` as a varint.
fn varint_len(mut n: u64) -> usize {
    let mut len = 1;
    while n >= 0x80 {
        n >>= 7;
        len += 1;
    }
    len
}

/// Number of values that `value` decodes into: every nested value and map key
/// counts, as in [`ValueBudget`].
fn count_values(value: &Value) -> usize {
    match value {
        Value::Array(items) => 1 + items.iter().map(count_values).sum::<usize>(),
        Value::Map(entries) => {
            1 + entries
                .iter()
                .map(|(key, value)| count_values(key) + count_values(value))
                .sum::<usize>()
        }
        Value::Nil
        | Value::Bool(_)
        | Value::Int(_)
        | Value::Float(_)
        | Value::String(_)
        | Value::FuncRef(_) => 1,
    }
}

/// Encodes an event as a complete frame, validating name and arguments first.
pub(crate) fn encode_event(
    tag: u32,
    name: &str,
    args: &[Value],
    max_frame: usize,
) -> Result<Arc<[u8]>, NetError> {
    validate_event_name(name)?;
    check_values(args)?;
    encode_frame(&EventFrame { tag, name, args }, max_frame)
}

/// Encodes a message as a complete frame with its length prefix.
pub(crate) fn encode_frame<T: Serialize>(
    message: &T,
    max_frame: usize,
) -> Result<Arc<[u8]>, NetError> {
    let buffer = postcard::to_extend(message, vec![0_u8; FRAME_HEADER_LEN])
        .map_err(|err| NetError::Protocol(format!("cannot encode message: {err}")))?;
    finish_frame(buffer, max_frame)
}

/// Writes the length prefix into the first [`FRAME_HEADER_LEN`] bytes of
/// `buffer`, which hold the payload after them, and checks the payload size.
fn finish_frame(mut buffer: Vec<u8>, max_frame: usize) -> Result<Arc<[u8]>, NetError> {
    let len = buffer.len() - FRAME_HEADER_LEN;
    let too_large = NetError::FrameTooLarge {
        len,
        max: max_frame,
    };
    if len > max_frame {
        return Err(too_large);
    }
    let prefix = u32::try_from(len).map_err(|_| too_large)?;
    buffer[..FRAME_HEADER_LEN].copy_from_slice(&prefix.to_le_bytes());
    Ok(Arc::from(buffer))
}

/// Decodes the payload of a message sent by a client. Rejects trailing bytes
/// and messages with more than [`MAX_DECODED_VALUES`] values.
pub(crate) fn decode_client_message(payload: &[u8]) -> Result<ClientMessage, NetError> {
    let budget = ValueBudget::new();
    decode_budgeted(payload, &budget, ClientMessageSeed { budget: &budget })
}

/// Decodes the payload of a message sent by the server. Rejects trailing bytes
/// and messages with more than [`MAX_DECODED_VALUES`] values.
pub(crate) fn decode_server_message(payload: &[u8]) -> Result<ServerMessage, NetError> {
    let budget = ValueBudget::new();
    decode_budgeted(payload, &budget, ServerMessageSeed { budget: &budget })
}

/// Runs `seed` over `payload`, which must hold exactly one message.
fn decode_budgeted<'de, S: DeserializeSeed<'de>>(
    payload: &'de [u8],
    budget: &ValueBudget,
    seed: S,
) -> Result<S::Value, NetError> {
    let mut deserializer = postcard::Deserializer::from_bytes(payload);
    let message = seed.deserialize(&mut deserializer).map_err(|err| {
        if budget.exceeded.get() {
            NetError::Protocol(format!(
                "message decodes into more than {MAX_DECODED_VALUES} values"
            ))
        } else {
            NetError::Protocol(format!("malformed message: {err}"))
        }
    })?;
    let rest = deserializer
        .finalize()
        .map_err(|err| NetError::Protocol(format!("malformed message: {err}")))?;
    if rest.is_empty() {
        Ok(message)
    } else {
        Err(NetError::Protocol(
            "trailing bytes after message".to_owned(),
        ))
    }
}

/// Reads one frame payload. Returns `Ok(None)` when the peer finished the
/// stream cleanly between frames, and [`NetError::Closed`] when the stream
/// failed for a transport reason.
///
/// This is not cancel-safe: a partially read frame is lost if the future is
/// dropped, so callers must keep it running in its own task or select arm.
pub(crate) async fn read_frame(
    recv: &mut RecvStream,
    max_frame: usize,
) -> Result<Option<Vec<u8>>, NetError> {
    let mut header = [0_u8; FRAME_HEADER_LEN];
    match recv.read_exact(&mut header).await {
        Ok(()) => {}
        Err(ReadExactError::FinishedEarly(0)) => return Ok(None),
        Err(ReadExactError::FinishedEarly(_)) => {
            return Err(NetError::Protocol(
                "stream ended inside a frame header".to_owned(),
            ));
        }
        Err(ReadExactError::ReadError(_)) => return Err(NetError::Closed),
    }
    let len = usize::try_from(u32::from_le_bytes(header)).unwrap_or(usize::MAX);
    if len > max_frame {
        return Err(NetError::FrameTooLarge {
            len,
            max: max_frame,
        });
    }
    let mut payload = vec![0_u8; len];
    match recv.read_exact(&mut payload).await {
        Ok(()) => Ok(Some(payload)),
        Err(ReadExactError::FinishedEarly(_)) => {
            Err(NetError::Protocol("stream ended inside a frame".to_owned()))
        }
        Err(ReadExactError::ReadError(_)) => Err(NetError::Closed),
    }
}

/// Borrowed sequence of values.
struct Elements<'a>(&'a [Value]);

impl Serialize for Elements<'_> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut seq = serializer.serialize_seq(Some(self.0.len()))?;
        for value in self.0 {
            seq.serialize_element(&Encode(value))?;
        }
        seq.end()
    }
}

/// Borrowed encoding of one value.
struct Encode<'a>(&'a Value);

impl Serialize for Encode<'_> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match self.0 {
            Value::Nil => serializer.serialize_unit_variant(VALUE_NAME, TAG_NIL, "Nil"),
            Value::Bool(v) => serializer.serialize_newtype_variant(VALUE_NAME, TAG_BOOL, "Bool", v),
            Value::Int(v) => serializer.serialize_newtype_variant(VALUE_NAME, TAG_INT, "Int", v),
            Value::Float(v) => {
                serializer.serialize_newtype_variant(VALUE_NAME, TAG_FLOAT, "Float", v)
            }
            Value::String(bytes) => serializer.serialize_newtype_variant(
                VALUE_NAME,
                TAG_STRING,
                "String",
                &Bytes(bytes),
            ),
            Value::Array(items) => serializer.serialize_newtype_variant(
                VALUE_NAME,
                TAG_ARRAY,
                "Array",
                &Elements(items),
            ),
            Value::Map(entries) => {
                serializer.serialize_newtype_variant(VALUE_NAME, TAG_MAP, "Map", &Pairs(entries))
            }
            Value::FuncRef(_) => Err(serde::ser::Error::custom(
                "function references cannot be sent over the network",
            )),
        }
    }
}

struct Bytes<'a>(&'a [u8]);

impl Serialize for Bytes<'_> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_bytes(self.0)
    }
}

struct Pairs<'a>(&'a [(Value, Value)]);

impl Serialize for Pairs<'_> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut seq = serializer.serialize_seq(Some(self.0.len()))?;
        for (key, value) in self.0 {
            seq.serialize_element(&(Encode(key), Encode(value)))?;
        }
        seq.end()
    }
}

/// Counts the values that one message decodes into. Every value costs one
/// unit, map keys included, and the count is checked before the value is
/// built, so the decoder never holds more than [`MAX_DECODED_VALUES`] values.
#[derive(Debug)]
struct ValueBudget {
    remaining: Cell<usize>,
    /// Set when the budget ran out, so the caller can name the real cause of
    /// the failure instead of postcard's generic error.
    exceeded: Cell<bool>,
}

impl ValueBudget {
    fn new() -> Self {
        Self {
            remaining: Cell::new(MAX_DECODED_VALUES),
            exceeded: Cell::new(false),
        }
    }

    /// Spends one unit of the budget.
    fn spend<E: de::Error>(&self) -> Result<(), E> {
        if let Some(remaining) = self.remaining.get().checked_sub(1) {
            self.remaining.set(remaining);
            Ok(())
        } else {
            self.exceeded.set(true);
            Err(E::custom("message decodes into too many values"))
        }
    }
}

/// Decodes a message sent by a client. The variant tags come from
/// [`ClientMessage`].
struct ClientMessageSeed<'a> {
    budget: &'a ValueBudget,
}

impl<'de> DeserializeSeed<'de> for ClientMessageSeed<'_> {
    type Value = ClientMessage;

    fn deserialize<D: Deserializer<'de>>(self, deserializer: D) -> Result<ClientMessage, D::Error> {
        deserializer.deserialize_enum(
            MESSAGE_NAME,
            CLIENT_VARIANTS,
            ClientMessageVisitor {
                budget: self.budget,
            },
        )
    }
}

struct ClientMessageVisitor<'a> {
    budget: &'a ValueBudget,
}

impl<'de> Visitor<'de> for ClientMessageVisitor<'_> {
    type Value = ClientMessage;

    fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
        formatter.write_str("a client message")
    }

    fn visit_enum<A: EnumAccess<'de>>(self, data: A) -> Result<ClientMessage, A::Error> {
        let (tag, variant): (u32, A::Variant) = data.variant()?;
        match tag {
            ClientMessage::HELLO_TAG => variant
                .newtype_variant::<(u32, String)>()
                .map(|(protocol, name)| ClientMessage::Hello { protocol, name }),
            ClientMessage::EVENT_TAG => variant
                .newtype_variant_seed(EventSeed {
                    budget: self.budget,
                })
                .map(|(name, args)| ClientMessage::Event { name, args }),
            ClientMessage::REQUEST_FILE_TAG => variant
                .newtype_variant::<FileHash>()
                .map(|hash| ClientMessage::RequestFile { hash }),
            other => Err(de::Error::unknown_variant(
                &other.to_string(),
                CLIENT_VARIANTS,
            )),
        }
    }
}

/// Decodes a message sent by the server. The variant tags come from
/// [`ServerMessage`].
struct ServerMessageSeed<'a> {
    budget: &'a ValueBudget,
}

impl<'de> DeserializeSeed<'de> for ServerMessageSeed<'_> {
    type Value = ServerMessage;

    fn deserialize<D: Deserializer<'de>>(self, deserializer: D) -> Result<ServerMessage, D::Error> {
        deserializer.deserialize_enum(
            MESSAGE_NAME,
            SERVER_VARIANTS,
            ServerMessageVisitor {
                budget: self.budget,
            },
        )
    }
}

struct ServerMessageVisitor<'a> {
    budget: &'a ValueBudget,
}

impl<'de> Visitor<'de> for ServerMessageVisitor<'_> {
    type Value = ServerMessage;

    fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
        formatter.write_str("a server message")
    }

    fn visit_enum<A: EnumAccess<'de>>(self, data: A) -> Result<ServerMessage, A::Error> {
        let (tag, variant): (u32, A::Variant) = data.variant()?;
        match tag {
            ServerMessage::WELCOME_TAG => {
                variant
                    .newtype_variant::<(u32, String)>()
                    .map(|(player, server_name)| ServerMessage::Welcome {
                        player,
                        server_name,
                    })
            }
            ServerMessage::REJECTED_TAG => variant
                .newtype_variant::<String>()
                .map(|reason| ServerMessage::Rejected { reason }),
            ServerMessage::EVENT_TAG => variant
                .newtype_variant_seed(EventSeed {
                    budget: self.budget,
                })
                .map(|(name, args)| ServerMessage::Event { name, args }),
            ServerMessage::KICKED_TAG => variant
                .newtype_variant::<String>()
                .map(|reason| ServerMessage::Kicked { reason }),
            ServerMessage::STATE_BAG_TAG => variant
                .newtype_variant_seed(StateSeed {
                    budget: self.budget,
                })
                .map(|(bag, key, value)| ServerMessage::StateBag {
                    bag,
                    key,
                    value: StateValue(value),
                }),
            ServerMessage::STATE_BAG_REMOVED_TAG => variant
                .newtype_variant::<String>()
                .map(|bag| ServerMessage::StateBagRemoved { bag }),
            ServerMessage::STATE_SNAPSHOT_TAG => variant
                .newtype_variant_seed(SnapshotSeed {
                    budget: self.budget,
                })
                .map(|entries| ServerMessage::StateSnapshot { entries }),
            ServerMessage::RESOURCES_TAG => variant
                .newtype_variant_seed(ResourcesSeed {
                    budget: self.budget,
                })
                .map(|entries| ServerMessage::Resources { entries }),
            ServerMessage::RESOURCE_STARTED_TAG => variant
                .newtype_variant_seed(ResourceEntrySeed {
                    budget: self.budget,
                })
                .map(|entry| ServerMessage::ResourceStarted { entry }),
            ServerMessage::RESOURCE_STOPPED_TAG => variant
                .newtype_variant::<String>()
                .and_then(|name| checked_name::<A::Error>(name, MAX_RESOURCE_NAME_LEN))
                .map(|name| ServerMessage::ResourceStopped { name }),
            ServerMessage::FILE_UNAVAILABLE_TAG => variant
                .newtype_variant::<FileHash>()
                .map(|hash| ServerMessage::FileUnavailable { hash }),
            other => Err(de::Error::unknown_variant(
                &other.to_string(),
                SERVER_VARIANTS,
            )),
        }
    }
}

/// Decodes the body of a state change, `(bag, key, value)`.
#[derive(Clone, Copy)]
struct StateSeed<'a> {
    budget: &'a ValueBudget,
}

impl<'de> DeserializeSeed<'de> for StateSeed<'_> {
    type Value = (String, String, Value);

    fn deserialize<D: Deserializer<'de>>(
        self,
        deserializer: D,
    ) -> Result<(String, String, Value), D::Error> {
        deserializer.deserialize_tuple(
            3,
            StateVisitor {
                budget: self.budget,
            },
        )
    }
}

struct StateVisitor<'a> {
    budget: &'a ValueBudget,
}

impl<'de> Visitor<'de> for StateVisitor<'_> {
    type Value = (String, String, Value);

    fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
        formatter.write_str("a state bag name, a key and a value")
    }

    fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Self::Value, A::Error> {
        let bag: String = seq
            .next_element()?
            .ok_or_else(|| de::Error::invalid_length(0, &"a state bag name"))?;
        let key: String = seq
            .next_element()?
            .ok_or_else(|| de::Error::invalid_length(1, &"a state key"))?;
        let value = seq
            .next_element_seed(ValueSeed {
                depth: 1,
                budget: self.budget,
            })?
            .ok_or_else(|| de::Error::invalid_length(2, &"a state value"))?;
        Ok((bag, key, value))
    }
}

/// Decodes the entries of a snapshot, a sequence of state changes. Each entry
/// spends at least one unit of the budget, so the sequence is bounded by it.
#[derive(Clone, Copy)]
struct SnapshotSeed<'a> {
    budget: &'a ValueBudget,
}

impl<'de> DeserializeSeed<'de> for SnapshotSeed<'_> {
    type Value = Vec<(String, String, StateValue)>;

    fn deserialize<D: Deserializer<'de>>(
        self,
        deserializer: D,
    ) -> Result<Vec<(String, String, StateValue)>, D::Error> {
        deserializer.deserialize_seq(SnapshotVisitor {
            budget: self.budget,
        })
    }
}

struct SnapshotVisitor<'a> {
    budget: &'a ValueBudget,
}

impl<'de> Visitor<'de> for SnapshotVisitor<'_> {
    type Value = Vec<(String, String, StateValue)>;

    fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
        formatter.write_str("a sequence of state changes")
    }

    fn visit_seq<A: SeqAccess<'de>>(
        self,
        mut seq: A,
    ) -> Result<Vec<(String, String, StateValue)>, A::Error> {
        let mut entries = Vec::new();
        while let Some((bag, key, value)) = seq.next_element_seed(StateSeed {
            budget: self.budget,
        })? {
            entries.push((bag, key, StateValue(value)));
        }
        Ok(entries)
    }
}

/// Decodes the body of a `Resources` frame: a sequence of resource entries.
#[derive(Clone, Copy)]
struct ResourcesSeed<'a> {
    budget: &'a ValueBudget,
}

impl<'de> DeserializeSeed<'de> for ResourcesSeed<'_> {
    type Value = Vec<ResourceEntry>;

    fn deserialize<D: Deserializer<'de>>(
        self,
        deserializer: D,
    ) -> Result<Vec<ResourceEntry>, D::Error> {
        deserializer.deserialize_seq(ResourcesVisitor {
            budget: self.budget,
        })
    }
}

struct ResourcesVisitor<'a> {
    budget: &'a ValueBudget,
}

impl<'de> Visitor<'de> for ResourcesVisitor<'_> {
    type Value = Vec<ResourceEntry>;

    fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
        formatter.write_str("a sequence of resource entries")
    }

    fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Vec<ResourceEntry>, A::Error> {
        let mut entries = Vec::new();
        while let Some(entry) = seq.next_element_seed(ResourceEntrySeed {
            budget: self.budget,
        })? {
            entries.push(entry);
        }
        Ok(entries)
    }
}

/// Decodes one resource entry, `(name, files)`. The entry costs one unit of
/// the budget, and each file costs one more.
#[derive(Clone, Copy)]
struct ResourceEntrySeed<'a> {
    budget: &'a ValueBudget,
}

impl<'de> DeserializeSeed<'de> for ResourceEntrySeed<'_> {
    type Value = ResourceEntry;

    fn deserialize<D: Deserializer<'de>>(self, deserializer: D) -> Result<ResourceEntry, D::Error> {
        self.budget.spend::<D::Error>()?;
        deserializer.deserialize_tuple(
            2,
            ResourceEntryVisitor {
                budget: self.budget,
            },
        )
    }
}

struct ResourceEntryVisitor<'a> {
    budget: &'a ValueBudget,
}

impl<'de> Visitor<'de> for ResourceEntryVisitor<'_> {
    type Value = ResourceEntry;

    fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
        formatter.write_str("a resource name and its files")
    }

    fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<ResourceEntry, A::Error> {
        let name: String = seq
            .next_element()?
            .ok_or_else(|| de::Error::invalid_length(0, &"a resource name"))?;
        let name = checked_name::<A::Error>(name, MAX_RESOURCE_NAME_LEN)?;
        let files = seq
            .next_element_seed(FilesSeed {
                budget: self.budget,
            })?
            .ok_or_else(|| de::Error::invalid_length(1, &"a list of files"))?;
        Ok(ResourceEntry { name, files })
    }
}

/// Decodes a list of files, each of which costs one unit of the budget.
#[derive(Clone, Copy)]
struct FilesSeed<'a> {
    budget: &'a ValueBudget,
}

impl<'de> DeserializeSeed<'de> for FilesSeed<'_> {
    type Value = Vec<FileEntry>;

    fn deserialize<D: Deserializer<'de>>(
        self,
        deserializer: D,
    ) -> Result<Vec<FileEntry>, D::Error> {
        deserializer.deserialize_seq(FilesVisitor {
            budget: self.budget,
        })
    }
}

struct FilesVisitor<'a> {
    budget: &'a ValueBudget,
}

impl<'de> Visitor<'de> for FilesVisitor<'_> {
    type Value = Vec<FileEntry>;

    fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
        formatter.write_str("a list of files")
    }

    fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Vec<FileEntry>, A::Error> {
        let mut files = Vec::new();
        while let Some(file) = seq.next_element_seed(FileEntrySeed {
            budget: self.budget,
        })? {
            files.push(file);
        }
        Ok(files)
    }
}

/// Decodes one file entry, `(name, kind, hash, size)`.
#[derive(Clone, Copy)]
struct FileEntrySeed<'a> {
    budget: &'a ValueBudget,
}

impl<'de> DeserializeSeed<'de> for FileEntrySeed<'_> {
    type Value = FileEntry;

    fn deserialize<D: Deserializer<'de>>(self, deserializer: D) -> Result<FileEntry, D::Error> {
        self.budget.spend::<D::Error>()?;
        deserializer.deserialize_tuple(4, FileEntryVisitor)
    }
}

struct FileEntryVisitor;

impl<'de> Visitor<'de> for FileEntryVisitor {
    type Value = FileEntry;

    fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
        formatter.write_str("a file name, kind, hash and size")
    }

    fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<FileEntry, A::Error> {
        let name: String = seq
            .next_element()?
            .ok_or_else(|| de::Error::invalid_length(0, &"a file name"))?;
        let name = checked_name::<A::Error>(name, MAX_FILE_NAME_LEN)?;
        let kind: u8 = seq
            .next_element()?
            .ok_or_else(|| de::Error::invalid_length(1, &"a file kind"))?;
        if kind > FILE_KIND_FILE {
            return Err(de::Error::custom(format!("unknown file kind {kind}")));
        }
        let hash: FileHash = seq
            .next_element()?
            .ok_or_else(|| de::Error::invalid_length(2, &"a file hash"))?;
        let size: u64 = seq
            .next_element()?
            .ok_or_else(|| de::Error::invalid_length(3, &"a file size"))?;
        if size > MAX_FILE_SIZE {
            return Err(de::Error::custom(format!(
                "file size {size} is over the limit"
            )));
        }
        Ok(FileEntry {
            name,
            kind,
            hash,
            size,
        })
    }
}

/// Checks a decoded name against its limit, as the encoder does.
fn checked_name<E: de::Error>(name: String, max: usize) -> Result<String, E> {
    if name.is_empty() || name.len() > max {
        Err(E::custom(format!("name must be 1 to {max} bytes")))
    } else {
        Ok(name)
    }
}

/// Decodes the body of an event, `(name, args)`. Both message enums carry
/// their events in this layout.
#[derive(Clone, Copy)]
struct EventSeed<'a> {
    budget: &'a ValueBudget,
}

impl<'de> DeserializeSeed<'de> for EventSeed<'_> {
    type Value = (String, Args);

    fn deserialize<D: Deserializer<'de>>(
        self,
        deserializer: D,
    ) -> Result<(String, Args), D::Error> {
        deserializer.deserialize_tuple(
            2,
            EventVisitor {
                budget: self.budget,
            },
        )
    }
}

struct EventVisitor<'a> {
    budget: &'a ValueBudget,
}

impl<'de> Visitor<'de> for EventVisitor<'_> {
    type Value = (String, Args);

    fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
        formatter.write_str("an event name and its arguments")
    }

    fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<(String, Args), A::Error> {
        let name: String = seq
            .next_element()?
            .ok_or_else(|| de::Error::invalid_length(0, &"an event name"))?;
        let args = seq
            .next_element_seed(ValuesSeed {
                child_depth: 1,
                budget: self.budget,
            })?
            .ok_or_else(|| de::Error::invalid_length(1, &"an argument list"))?;
        Ok((name, Args(args)))
    }
}

/// Decodes one value whose nesting depth is `depth`.
#[derive(Clone, Copy)]
struct ValueSeed<'a> {
    depth: usize,
    budget: &'a ValueBudget,
}

impl<'de> DeserializeSeed<'de> for ValueSeed<'_> {
    type Value = Value;

    fn deserialize<D: Deserializer<'de>>(self, deserializer: D) -> Result<Value, D::Error> {
        if self.depth > MAX_VALUE_DEPTH {
            return Err(de::Error::custom("value nested too deeply"));
        }
        self.budget.spend::<D::Error>()?;
        deserializer.deserialize_enum(
            VALUE_NAME,
            VALUE_VARIANTS,
            ValueVisitor {
                depth: self.depth,
                budget: self.budget,
            },
        )
    }
}

struct ValueVisitor<'a> {
    depth: usize,
    budget: &'a ValueBudget,
}

impl<'de> Visitor<'de> for ValueVisitor<'_> {
    type Value = Value;

    fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
        formatter.write_str("a script value")
    }

    fn visit_enum<A: EnumAccess<'de>>(self, data: A) -> Result<Value, A::Error> {
        let (tag, variant): (u32, A::Variant) = data.variant()?;
        let child_depth = self.depth + 1;
        match tag {
            TAG_NIL => {
                variant.unit_variant()?;
                Ok(Value::Nil)
            }
            TAG_BOOL => variant.newtype_variant().map(Value::Bool),
            TAG_INT => variant.newtype_variant().map(Value::Int),
            TAG_FLOAT => variant.newtype_variant().map(Value::Float),
            TAG_STRING => variant.newtype_variant_seed(BytesSeed).map(Value::String),
            TAG_ARRAY => variant
                .newtype_variant_seed(ValuesSeed {
                    child_depth,
                    budget: self.budget,
                })
                .map(Value::Array),
            TAG_MAP => variant
                .newtype_variant_seed(PairsSeed {
                    child_depth,
                    budget: self.budget,
                })
                .map(Value::Map),
            other => Err(de::Error::unknown_variant(
                &other.to_string(),
                VALUE_VARIANTS,
            )),
        }
    }
}

struct BytesSeed;

impl<'de> DeserializeSeed<'de> for BytesSeed {
    type Value = Box<[u8]>;

    fn deserialize<D: Deserializer<'de>>(self, deserializer: D) -> Result<Box<[u8]>, D::Error> {
        deserializer.deserialize_bytes(BytesVisitor)
    }
}

struct BytesVisitor;

impl Visitor<'_> for BytesVisitor {
    type Value = Box<[u8]>;

    fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
        formatter.write_str("a byte string")
    }

    fn visit_bytes<E: de::Error>(self, bytes: &[u8]) -> Result<Box<[u8]>, E> {
        Ok(bytes.into())
    }
}

/// Decodes a sequence of values, each at depth `child_depth`.
#[derive(Clone, Copy)]
struct ValuesSeed<'a> {
    child_depth: usize,
    budget: &'a ValueBudget,
}

impl<'de> DeserializeSeed<'de> for ValuesSeed<'_> {
    type Value = Vec<Value>;

    fn deserialize<D: Deserializer<'de>>(self, deserializer: D) -> Result<Vec<Value>, D::Error> {
        deserializer.deserialize_seq(ValuesVisitor {
            child_depth: self.child_depth,
            budget: self.budget,
        })
    }
}

struct ValuesVisitor<'a> {
    child_depth: usize,
    budget: &'a ValueBudget,
}

impl<'de> Visitor<'de> for ValuesVisitor<'_> {
    type Value = Vec<Value>;

    fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
        formatter.write_str("a sequence of script values")
    }

    fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Vec<Value>, A::Error> {
        let mut values = Vec::new();
        while let Some(value) = seq.next_element_seed(ValueSeed {
            depth: self.child_depth,
            budget: self.budget,
        })? {
            values.push(value);
        }
        Ok(values)
    }
}

/// Decodes a sequence of key/value pairs, each element at depth `child_depth`.
#[derive(Clone, Copy)]
struct PairsSeed<'a> {
    child_depth: usize,
    budget: &'a ValueBudget,
}

impl<'de> DeserializeSeed<'de> for PairsSeed<'_> {
    type Value = Vec<(Value, Value)>;

    fn deserialize<D: Deserializer<'de>>(
        self,
        deserializer: D,
    ) -> Result<Vec<(Value, Value)>, D::Error> {
        deserializer.deserialize_seq(PairsVisitor {
            child_depth: self.child_depth,
            budget: self.budget,
        })
    }
}

struct PairsVisitor<'a> {
    child_depth: usize,
    budget: &'a ValueBudget,
}

impl<'de> Visitor<'de> for PairsVisitor<'_> {
    type Value = Vec<(Value, Value)>;

    fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
        formatter.write_str("a sequence of key/value pairs")
    }

    fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Vec<(Value, Value)>, A::Error> {
        let mut entries = Vec::new();
        while let Some(pair) = seq.next_element_seed(PairSeed {
            child_depth: self.child_depth,
            budget: self.budget,
        })? {
            entries.push(pair);
        }
        Ok(entries)
    }
}

#[derive(Clone, Copy)]
struct PairSeed<'a> {
    child_depth: usize,
    budget: &'a ValueBudget,
}

impl<'de> DeserializeSeed<'de> for PairSeed<'_> {
    type Value = (Value, Value);

    fn deserialize<D: Deserializer<'de>>(
        self,
        deserializer: D,
    ) -> Result<(Value, Value), D::Error> {
        deserializer.deserialize_tuple(
            2,
            PairVisitor {
                child_depth: self.child_depth,
                budget: self.budget,
            },
        )
    }
}

struct PairVisitor<'a> {
    child_depth: usize,
    budget: &'a ValueBudget,
}

impl<'de> Visitor<'de> for PairVisitor<'_> {
    type Value = (Value, Value);

    fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
        formatter.write_str("a key/value pair")
    }

    fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<(Value, Value), A::Error> {
        let depth = self.child_depth;
        let key = seq
            .next_element_seed(ValueSeed {
                depth,
                budget: self.budget,
            })?
            .ok_or_else(|| de::Error::invalid_length(0, &"a key"))?;
        let value = seq
            .next_element_seed(ValueSeed {
                depth,
                budget: self.budget,
            })?
            .ok_or_else(|| de::Error::invalid_length(1, &"a value"))?;
        Ok((key, value))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::files::{FILE_KIND_CLIENT, FILE_KIND_SHARED};
    use crate::protocol::MAX_FRAME;

    /// The payload of `message` encoded as a complete frame.
    fn payload_of<T: Serialize>(message: &T) -> Vec<u8> {
        let frame = encode_frame(message, MAX_FRAME).expect("message encodes");
        frame[FRAME_HEADER_LEN..].to_vec()
    }

    fn event_with(args: Vec<Value>) -> ClientMessage {
        ClientMessage::Event {
            name: "test".to_owned(),
            args: Args(args),
        }
    }

    #[test]
    fn sixty_thousand_values_decode() {
        let payload = payload_of(&event_with(vec![Value::Nil; 60_000]));
        assert!(decode_client_message(&payload).is_ok());
    }

    #[test]
    fn seventy_thousand_values_are_rejected() {
        let payload = payload_of(&event_with(vec![Value::Nil; 70_000]));
        assert!(matches!(
            decode_client_message(&payload),
            Err(NetError::Protocol(reason)) if reason.contains("values")
        ));
    }

    #[test]
    fn map_keys_count_against_the_budget() {
        // 40 000 pairs are 80 000 values, keys and values alike.
        let entries = vec![(Value::Nil, Value::Nil); 40_000];
        let payload = payload_of(&event_with(vec![Value::Map(entries)]));
        assert!(matches!(
            decode_client_message(&payload),
            Err(NetError::Protocol(_))
        ));
    }

    #[test]
    fn every_message_variant_decodes_to_itself() {
        let hello = ClientMessage::Hello {
            protocol: 1,
            name: "ann".to_owned(),
        };
        assert!(matches!(
            decode_client_message(&payload_of(&hello)),
            Ok(ClientMessage::Hello { protocol: 1, name }) if name == "ann"
        ));
        let event = event_with(vec![Value::Int(3)]);
        assert!(matches!(
            decode_client_message(&payload_of(&event)),
            Ok(ClientMessage::Event { name, args }) if name == "test" && args.0 == [Value::Int(3)]
        ));

        let welcome = ServerMessage::Welcome {
            player: 7,
            server_name: "srv".to_owned(),
        };
        assert!(matches!(
            decode_server_message(&payload_of(&welcome)),
            Ok(ServerMessage::Welcome { player: 7, server_name }) if server_name == "srv"
        ));
        let rejected = ServerMessage::Rejected {
            reason: "full".to_owned(),
        };
        assert!(matches!(
            decode_server_message(&payload_of(&rejected)),
            Ok(ServerMessage::Rejected { reason }) if reason == "full"
        ));
        let event = ServerMessage::Event {
            name: "e".to_owned(),
            args: Args(vec![Value::Bool(true)]),
        };
        assert!(matches!(
            decode_server_message(&payload_of(&event)),
            Ok(ServerMessage::Event { name, args }) if name == "e" && args.0 == [Value::Bool(true)]
        ));
        let kicked = ServerMessage::Kicked {
            reason: "bye".to_owned(),
        };
        assert!(matches!(
            decode_server_message(&payload_of(&kicked)),
            Ok(ServerMessage::Kicked { reason }) if reason == "bye"
        ));
        let removed = ServerMessage::StateBagRemoved {
            bag: "player:4".to_owned(),
        };
        assert!(matches!(
            decode_server_message(&payload_of(&removed)),
            Ok(ServerMessage::StateBagRemoved { bag }) if bag == "player:4"
        ));
    }

    #[test]
    fn state_frames_decode_to_the_change_they_carry() {
        let value = Value::Map(vec![(Value::from("k"), Value::Float(1.5))]);
        let frame = encode_state("global", "score", &value, MAX_FRAME).expect("state encodes");
        let decoded = decode_server_message(&frame[FRAME_HEADER_LEN..]);
        assert!(matches!(
            decoded,
            Ok(ServerMessage::StateBag { bag, key, value: StateValue(got) })
                if bag == "global" && key == "score" && got == value
        ));

        let frame = encode_state_removed("player:4", MAX_FRAME).expect("removal encodes");
        assert!(matches!(
            decode_server_message(&frame[FRAME_HEADER_LEN..]),
            Ok(ServerMessage::StateBagRemoved { bag }) if bag == "player:4"
        ));
    }

    #[test]
    fn state_names_are_limited_like_event_names() {
        let long = "x".repeat(MAX_STATE_NAME_LEN + 1);
        assert!(encode_state(&long, "key", &Value::Nil, MAX_FRAME).is_err());
        assert!(encode_state("global", "", &Value::Nil, MAX_FRAME).is_err());
        assert!(encode_state("global", &long, &Value::Nil, MAX_FRAME).is_err());
        assert!(encode_state_removed("", MAX_FRAME).is_err());
    }

    #[test]
    fn state_values_cannot_hold_functions() {
        let value = Value::Array(vec![Value::FuncRef(sixm_core::FuncRef {
            owner: Arc::from("r"),
            instance: 1,
            id: 1,
        })]);
        assert!(matches!(
            encode_state("global", "f", &value, MAX_FRAME),
            Err(NetError::FunctionNotSendable)
        ));
    }

    /// Decodes every frame of a snapshot, checking each frame's length prefix
    /// and size, and returns the entries in the order they arrived.
    fn decode_snapshot_frames(
        frames: &[Arc<[u8]>],
        max_frame: usize,
    ) -> Vec<(String, String, Value)> {
        let mut decoded = Vec::new();
        for frame in frames {
            let len = frame.len() - FRAME_HEADER_LEN;
            assert!(len <= max_frame, "frame of {len} bytes exceeds {max_frame}");
            let prefix = u32::try_from(len).expect("frame fits a u32");
            assert_eq!(&frame[..FRAME_HEADER_LEN], &prefix.to_le_bytes());
            match decode_server_message(&frame[FRAME_HEADER_LEN..]) {
                Ok(ServerMessage::StateSnapshot { entries }) => {
                    decoded.extend(
                        entries
                            .into_iter()
                            .map(|(bag, key, value)| (bag, key, value.0)),
                    );
                }
                Ok(_) => panic!("the frame is not a snapshot"),
                Err(err) => panic!("the frame does not decode: {err}"),
            }
        }
        decoded
    }

    #[test]
    fn state_snapshot_round_trips_in_order() {
        let first = Value::from("a");
        let second = Value::Map(vec![(Value::from("k"), Value::Float(2.5))]);
        let third = Value::Nil;
        let entries = [
            ("global", "x", &first),
            ("player:1", "y", &second),
            ("global", "z", &third),
        ];
        let frames = encode_state_snapshot(&entries, MAX_FRAME).expect("snapshot encodes");
        assert_eq!(frames.len(), 1);
        assert_eq!(
            decode_snapshot_frames(&frames, MAX_FRAME),
            vec![
                ("global".to_owned(), "x".to_owned(), first),
                ("player:1".to_owned(), "y".to_owned(), second),
                ("global".to_owned(), "z".to_owned(), third),
            ]
        );
    }

    #[test]
    fn many_small_entries_split_into_frames_within_the_limit() {
        const MAX: usize = 512;
        let keys: Vec<String> = (0..3000).map(|i| format!("key{i}")).collect();
        let values: Vec<Value> = (0..3000_i64).map(Value::Int).collect();
        let entries: Vec<(&str, &str, &Value)> = keys
            .iter()
            .zip(&values)
            .map(|(key, value)| ("player:1", key.as_str(), value))
            .collect();

        let frames = encode_state_snapshot(&entries, MAX).expect("snapshot encodes");
        assert!(
            frames.len() > 1,
            "3000 entries cannot fit one 512-byte frame"
        );
        let expected: Vec<(String, String, Value)> = entries
            .iter()
            .map(|&(bag, key, value)| (bag.to_owned(), key.to_owned(), value.clone()))
            .collect();
        assert_eq!(decode_snapshot_frames(&frames, MAX), expected);
    }

    #[test]
    fn snapshot_frames_respect_the_decoding_budget() {
        // Each entry decodes into 10 001 values, so six fit under the budget of
        // 65 536 and ten entries need exactly two frames.
        let value = Value::Array(vec![Value::Nil; 10_000]);
        let keys: Vec<String> = (0..10).map(|i| format!("key{i}")).collect();
        let entries: Vec<(&str, &str, &Value)> = keys
            .iter()
            .map(|key| ("global", key.as_str(), &value))
            .collect();

        let frames = encode_state_snapshot(&entries, MAX_FRAME).expect("snapshot encodes");
        assert_eq!(frames.len(), 2);
        assert_eq!(decode_snapshot_frames(&frames, MAX_FRAME).len(), 10);
    }

    #[test]
    fn entries_too_large_for_one_frame_are_skipped() {
        let small = Value::Int(1);
        let big = Value::from("x".repeat(100).as_str());
        let entries = [("global", "big", &big), ("global", "small", &small)];

        let frames = encode_state_snapshot(&entries, 64).expect("snapshot encodes");
        assert_eq!(
            decode_snapshot_frames(&frames, 64),
            vec![("global".to_owned(), "small".to_owned(), small.clone())]
        );
    }

    #[test]
    fn entries_over_the_decoding_budget_are_skipped() {
        // Smaller than a frame in bytes, but more values than one message may hold.
        let small = Value::Int(1);
        let over = Value::Array(vec![Value::Nil; MAX_DECODED_VALUES]);
        let entries = [("global", "over", &over), ("global", "small", &small)];

        let frames = encode_state_snapshot(&entries, MAX_FRAME).expect("snapshot encodes");
        assert_eq!(
            decode_snapshot_frames(&frames, MAX_FRAME),
            vec![("global".to_owned(), "small".to_owned(), small.clone())]
        );
    }

    #[test]
    fn invalid_snapshot_entries_fail_the_whole_call() {
        let value = Value::Nil;
        assert!(encode_state_snapshot(&[("global", "", &value)], MAX_FRAME).is_err());
        let func = Value::Array(vec![Value::FuncRef(sixm_core::FuncRef {
            owner: Arc::from("r"),
            instance: 1,
            id: 1,
        })]);
        assert!(matches!(
            encode_state_snapshot(&[("global", "f", &func)], MAX_FRAME),
            Err(NetError::FunctionNotSendable)
        ));
    }

    #[test]
    fn trailing_bytes_are_rejected() {
        let mut payload = payload_of(&event_with(vec![Value::Nil]));
        payload.push(0);
        assert!(matches!(
            decode_client_message(&payload),
            Err(NetError::Protocol(reason)) if reason.contains("trailing")
        ));
    }

    fn file(name: &str, kind: u8, byte: u8) -> FileEntry {
        FileEntry {
            name: name.to_owned(),
            kind,
            hash: [byte; 32],
            size: u64::from(byte) * 100,
        }
    }

    #[test]
    fn file_request_round_trips() {
        let request = ClientMessage::RequestFile { hash: [7; 32] };
        assert!(matches!(
            decode_client_message(&payload_of(&request)),
            Ok(ClientMessage::RequestFile { hash }) if hash == [7; 32]
        ));
    }

    #[test]
    fn resource_messages_round_trip() {
        let entry = ResourceEntry {
            name: "hello".to_owned(),
            files: vec![file("client/main.lua", FILE_KIND_CLIENT, 1)],
        };
        let started = ServerMessage::ResourceStarted {
            entry: entry.clone(),
        };
        assert!(matches!(
            decode_server_message(&payload_of(&started)),
            Ok(ServerMessage::ResourceStarted { entry: got }) if got == entry
        ));
        let stopped = ServerMessage::ResourceStopped {
            name: "hello".to_owned(),
        };
        assert!(matches!(
            decode_server_message(&payload_of(&stopped)),
            Ok(ServerMessage::ResourceStopped { name }) if name == "hello"
        ));
        let unavailable = ServerMessage::FileUnavailable { hash: [9; 32] };
        assert!(matches!(
            decode_server_message(&payload_of(&unavailable)),
            Ok(ServerMessage::FileUnavailable { hash }) if hash == [9; 32]
        ));
        let whole = ServerMessage::Resources {
            entries: vec![entry.clone()],
        };
        assert!(matches!(
            decode_server_message(&payload_of(&whole)),
            Ok(ServerMessage::Resources { entries }) if entries == [entry]
        ));
    }

    #[test]
    fn unknown_file_kind_is_rejected() {
        let entry = ResourceEntry {
            name: "bad".to_owned(),
            files: vec![file("x.lua", 9, 1)],
        };
        let payload = payload_of(&ServerMessage::ResourceStarted { entry });
        assert!(matches!(
            decode_server_message(&payload),
            Err(NetError::Protocol(_))
        ));
    }

    /// Decodes every frame of a `Resources` snapshot. Consecutive entries with
    /// the same name are merged, as the client does, and the empty frame that
    /// ends the snapshot must come last.
    fn decode_resources_snapshot(frames: &[Arc<[u8]>], max_frame: usize) -> Vec<ResourceEntry> {
        let mut merged: Vec<ResourceEntry> = Vec::new();
        for (index, frame) in frames.iter().enumerate() {
            let len = frame.len() - FRAME_HEADER_LEN;
            assert!(len <= max_frame, "frame of {len} bytes exceeds {max_frame}");
            let entries = match decode_server_message(&frame[FRAME_HEADER_LEN..]) {
                Ok(ServerMessage::Resources { entries }) => entries,
                other => panic!("frame {index} is not a resources frame: {other:?}"),
            };
            if index == frames.len() - 1 {
                assert!(entries.is_empty(), "the last frame must be empty");
                break;
            }
            assert!(!entries.is_empty(), "only the last frame may be empty");
            for entry in entries {
                match merged.last_mut() {
                    Some(last) if last.name == entry.name => last.files.extend(entry.files),
                    _ => merged.push(entry),
                }
            }
        }
        merged
    }

    #[test]
    fn resources_snapshot_round_trips_across_frames() {
        // One entry of 256 files is about 12 KiB, so each frame holds one entry.
        const MAX: usize = 16 * 1024;
        let many: Vec<FileEntry> = (0..600)
            .map(|i| {
                let byte = u8::try_from(i % 200).expect("fits a byte");
                file(&format!("file{i}.lua"), FILE_KIND_FILE, byte)
            })
            .collect();
        let resources = vec![
            ResourceEntry {
                name: "empty".to_owned(),
                files: Vec::new(),
            },
            ResourceEntry {
                name: "big".to_owned(),
                files: many,
            },
            ResourceEntry {
                name: "after".to_owned(),
                files: vec![file("a.lua", FILE_KIND_SHARED, 3)],
            },
        ];
        let frames = encode_resources_snapshot(&resources, MAX).expect("snapshot encodes");
        assert!(frames.len() > 2, "the snapshot must span several frames");
        assert_eq!(decode_resources_snapshot(&frames, MAX), resources);
    }

    #[test]
    fn empty_snapshot_is_one_terminating_frame() {
        let frames = encode_resources_snapshot(&[], MAX_FRAME).expect("snapshot encodes");
        assert_eq!(frames.len(), 1);
        assert_eq!(
            decode_resources_snapshot(&frames, MAX_FRAME),
            Vec::<ResourceEntry>::new()
        );
    }

    #[test]
    fn snapshot_within_the_decoding_budget_decodes() {
        // 300 resources of 256 files are about 2.7 MB and 77 000 decoded values,
        // so they need several frames whatever the byte limit.
        let resources: Vec<ResourceEntry> = (0..300)
            .map(|i| ResourceEntry {
                name: format!("res{i}"),
                files: (0..256).map(|_| file("f", FILE_KIND_CLIENT, 0)).collect(),
            })
            .collect();
        let frames = encode_resources_snapshot(&resources, MAX_FRAME).expect("snapshot encodes");
        assert_eq!(decode_resources_snapshot(&frames, MAX_FRAME), resources);
    }
}
