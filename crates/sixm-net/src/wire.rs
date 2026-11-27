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
use serde::Deserialize as _;
use serde::de::{
    self, DeserializeSeed, Deserializer, EnumAccess, SeqAccess, VariantAccess, Visitor,
};
use serde::ser::{SerializeSeq, SerializeStructVariant};
use serde::{Serialize, Serializer};
use sixm_core::{EntityKind, MAX_STATE_NAME_LEN, Value, is_finite};
use tracing::warn;

use crate::entities::{EntityEntry, EntityTransform};
use crate::error::NetError;
use crate::files::{
    FILE_KIND_FILE, FileEntry, FileHash, MAX_EXPORT_NAME_LEN, MAX_EXPORTS, MAX_FILE_NAME_LEN,
    MAX_FILE_SIZE, MAX_METADATA, MAX_METADATA_KEY_LEN, MAX_METADATA_VALUE_LEN,
    MAX_RESOURCE_NAME_LEN, ResourceEntry,
};
use crate::protocol::{
    ClientMessage, MAX_COMMAND_LEN, MAX_CONVAR_NAME_LEN, MAX_CONVAR_VALUE_LEN, MAX_DECODED_VALUES,
    MAX_DEFERRAL_MESSAGE_LEN, MAX_EVENT_NAME_LEN, MAX_LATENT_EVENT_LEN, MAX_REFUSAL_REASON_LEN,
    MAX_TRANSFORMS_PER_MESSAGE, MAX_VALUE_DEPTH, ServerMessage,
};

pub(crate) const FRAME_HEADER_LEN: usize = 4;
const MESSAGE_NAME: &str = "Message";
const VALUE_NAME: &str = "Value";
const TAG_NIL: u32 = 0;
const TAG_BOOL: u32 = 1;
const TAG_INT: u32 = 2;
const TAG_FLOAT: u32 = 3;
const TAG_STRING: u32 = 4;
const TAG_ARRAY: u32 = 5;
const TAG_MAP: u32 = 6;
const TAG_VECTOR2: u32 = 7;
const TAG_VECTOR3: u32 = 8;
const TAG_VECTOR4: u32 = 9;
const VALUE_VARIANTS: &[&str] = &[
    "Nil", "Bool", "Int", "Float", "String", "Array", "Map", "Vector2", "Vector3", "Vector4",
];
const CLIENT_VARIANTS: &[&str] = &[
    "Hello",
    "Event",
    "RequestFile",
    "Command",
    "EntityTransforms",
    "CreateEntity",
    "RequestEntityControl",
];
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
    "EntityCreated",
    "EntityTransforms",
    "EntityDeleted",
    "EntitySnapshot",
    "Convars",
    "Deferral",
    "EntityIdLease",
    "EntityCreateRefused",
    "EntityOwner",
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
        Value::Nil
        | Value::Bool(_)
        | Value::Int(_)
        | Value::Float(_)
        | Value::String(_)
        | Value::Vector2(_)
        | Value::Vector3(_)
        | Value::Vector4(_) => Ok(()),
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

/// The part of a [`ResourceEntry`] that one snapshot chunk carries: a name, a
/// run of its files and, in the first part only, its exports and metadata. Its
/// encoding is the encoding of a `ResourceEntry`.
#[derive(Serialize)]
struct EntryPart<'a> {
    name: &'a str,
    files: &'a [FileEntry],
    exports: &'a [String],
    metadata: &'a [(String, String)],
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
        for (index, files) in parts.into_iter().enumerate() {
            // The exports and metadata travel once, with the first part of the entry.
            let exports: &[String] = if index == 0 { &entry.exports } else { &[] };
            let metadata: &[(String, String)] = if index == 0 { &entry.metadata } else { &[] };
            let part = EntryPart {
                name: &entry.name,
                files,
                exports,
                metadata,
            };
            let encoded = postcard::to_extend(&part, Vec::<u8>::new())
                .map_err(|err| NetError::Protocol(format!("cannot encode message: {err}")))?;
            // The entry itself, each file, export and metadata pair count as one decoded value.
            let units = 1 + files.len() + exports.len() + metadata.len();
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

/// Bit of the `flags` byte of a transform: the position is present.
const TRANSFORM_POSITION: u8 = 0b01;
/// Bit of the `flags` byte of a transform: the rotation is present.
const TRANSFORM_ROTATION: u8 = 0b10;
/// Bits of the `flags` byte of a transform that are defined. Any other bit is an error.
const TRANSFORM_FLAGS: u8 = TRANSFORM_POSITION | TRANSFORM_ROTATION;
/// Centimetres per metre: the unit of a position on the wire.
const CENTIMETRES_PER_METRE: f32 = 100.0;
/// Rotation units in a full turn: an angle travels as a `u16` count of them.
const ANGLE_UNITS: f32 = 65_536.0;

/// Encodes entity transforms as the fewest frames that carry them in order.
/// Each update costs one unit of the decoding budget.
pub(crate) fn encode_entity_transforms(
    updates: &[EntityTransform],
    max_frame: usize,
) -> Result<Vec<Arc<[u8]>>, NetError> {
    encode_with(
        ServerMessage::ENTITY_TRANSFORMS_TAG,
        updates,
        max_frame,
        append_transform,
    )
}

/// Encodes the transforms a client sends as the fewest frames that carry them
/// in order. Each frame is one message and holds at most
/// [`MAX_TRANSFORMS_PER_MESSAGE`] updates.
pub(crate) fn encode_client_transforms(
    updates: &[EntityTransform],
    max_frame: usize,
) -> Result<Vec<Arc<[u8]>>, NetError> {
    let mut frames = Vec::new();
    for batch in updates.chunks(MAX_TRANSFORMS_PER_MESSAGE) {
        frames.extend(encode_with(
            ClientMessage::ENTITY_TRANSFORMS_TAG,
            batch,
            max_frame,
            append_transform,
        )?);
    }
    Ok(frames)
}

/// Appends one entity transform: the id as a varint, a flags byte, the
/// position and rotation the flags announce, and the epoch as a varint.
///
/// Each position component is a zigzag varint of centimetres, rounded and
/// clamped to the `i32` range. Each rotation angle is normalized to `[0, 360)`
/// and sent as a little-endian `u16` of [`ANGLE_UNITS`] per turn.
fn append_transform(
    bytes: &mut Vec<u8>,
    &(id, position, rotation, epoch): &EntityTransform,
) -> Result<(), NetError> {
    if position.is_none() && rotation.is_none() {
        return Err(NetError::InvalidInput(
            "an entity transform carries a position, a rotation or both".to_owned(),
        ));
    }
    if !position.is_none_or(is_finite) || !rotation.is_none_or(is_finite) {
        return Err(NetError::InvalidInput(
            "entity transforms must be finite".to_owned(),
        ));
    }
    let flags =
        position.map_or(0, |_| TRANSFORM_POSITION) | rotation.map_or(0, |_| TRANSFORM_ROTATION);
    push_varint(bytes, id);
    bytes.push(flags);
    if let Some(position) = position {
        for metres in position {
            push_zigzag(bytes, centimetres(metres));
        }
    }
    if let Some(rotation) = rotation {
        for degrees in rotation {
            bytes.extend_from_slice(&angle_units(degrees).to_le_bytes());
        }
    }
    push_varint(bytes, epoch);
    Ok(())
}

/// Appends `value` as a postcard varint, which is how a `u32` is encoded. The
/// bytes are written directly, since transforms are encoded once per player and
/// frame.
#[expect(
    clippy::cast_possible_truncation,
    reason = "each cast keeps the low byte of the value, which is what a varint stores"
)]
fn push_varint(bytes: &mut Vec<u8>, mut value: u32) {
    while value >= 0x80 {
        bytes.push((value & 0x7F) as u8 | 0x80);
        value >>= 7;
    }
    bytes.push(value as u8);
}

/// Appends `value` as a zigzag varint, which is how postcard encodes an `i32`.
#[expect(
    clippy::cast_sign_loss,
    reason = "zigzag maps the sign to the low bit, so the bits are read as unsigned on purpose"
)]
fn push_zigzag(bytes: &mut Vec<u8>, value: i32) {
    push_varint(bytes, ((value << 1) ^ (value >> 31)) as u32);
}

/// A position component in whole centimetres, rounded and clamped to the `i32` range.
#[expect(
    clippy::cast_possible_truncation,
    reason = "the value is clamped to the i32 range first, and the cast is then exact"
)]
fn centimetres(metres: f32) -> i32 {
    (metres * CENTIMETRES_PER_METRE)
        .round()
        .clamp(-2_147_483_648.0, 2_147_483_647.0) as i32
}

/// A rotation angle as a count of [`ANGLE_UNITS`] per turn. The angle is
/// normalized to `[0, 360)` first, so a turn of 360 is the count 0.
#[expect(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    reason = "the normalized angle gives a count of at most 65 536, and it is reduced below 65 536"
)]
fn angle_units(degrees: f32) -> u16 {
    let units = (degrees.rem_euclid(360.0) / 360.0 * ANGLE_UNITS).round() as u32;
    (units % 65_536) as u16
}

/// A position component in metres, from its whole centimetres.
#[expect(
    clippy::cast_precision_loss,
    reason = "centimetres beyond 2^24 lose their low bits, which a position can spare"
)]
fn metres(centimetres: i32) -> f32 {
    centimetres as f32 / CENTIMETRES_PER_METRE
}

/// A rotation angle in degrees, in `[0, 360)`, from its count of [`ANGLE_UNITS`] per turn.
fn degrees(units: u16) -> f32 {
    f32::from(units) * (360.0 / ANGLE_UNITS)
}

/// Encodes a list of entities as the fewest frames that carry them in order.
/// Each entry costs one unit of the decoding budget.
pub(crate) fn encode_entity_snapshot(
    entities: &[EntityEntry],
    max_frame: usize,
) -> Result<Vec<Arc<[u8]>>, NetError> {
    encode_items(ServerMessage::ENTITY_SNAPSHOT_TAG, entities, max_frame)
}

/// Encodes convars as the fewest frames that carry them in order. A `None`
/// value removes the convar on the client. Names must be 1 to
/// [`MAX_CONVAR_NAME_LEN`] bytes and values at most [`MAX_CONVAR_VALUE_LEN`],
/// and an entry that breaks either fails the whole call. Each entry costs one
/// unit of the decoding budget.
pub(crate) fn encode_convars(
    entries: &[(&str, Option<&str>)],
    max_frame: usize,
) -> Result<Vec<Arc<[u8]>>, NetError> {
    for &(name, value) in entries {
        validate_length("convar name", name, MAX_CONVAR_NAME_LEN)?;
        if let Some(value) = value {
            check_convar_value(value)?;
        }
    }
    encode_items(ServerMessage::CONVARS_TAG, entries, max_frame)
}

/// Checks that a convar value fits [`MAX_CONVAR_VALUE_LEN`].
fn check_convar_value(value: &str) -> Result<(), NetError> {
    if value.len() > MAX_CONVAR_VALUE_LEN {
        return Err(NetError::InvalidInput(format!(
            "convar value must be at most {MAX_CONVAR_VALUE_LEN} bytes, got {}",
            value.len()
        )));
    }
    Ok(())
}

/// Encodes `items` with their postcard encoding, as [`encode_with`] does.
fn encode_items<T: Serialize>(
    tag: u32,
    items: &[T],
    max_frame: usize,
) -> Result<Vec<Arc<[u8]>>, NetError> {
    encode_with(tag, items, max_frame, append_encoded)
}

/// Encodes `items` as the frames of a sequence message whose variant index is
/// `tag`, appending each item with `append`. Every item costs one unit of the
/// decoding budget. A frame that fails to seal, because one item is larger
/// than `max_frame`, fails the call.
fn encode_with<T>(
    tag: u32,
    items: &[T],
    max_frame: usize,
    append: fn(&mut Vec<u8>, &T) -> Result<(), NetError>,
) -> Result<Vec<Arc<[u8]>>, NetError> {
    let mut frames = Vec::new();
    let mut chunk = SnapshotChunk::new(tag);
    for item in items {
        // The item is encoded straight into the chunk's bytes, so encoding a
        // batch allocates only when the chunk grows.
        let start = chunk.bytes.len();
        append(&mut chunk.bytes, item)?;
        if !chunk.is_empty() && !chunk.fits(chunk.bytes.len(), 1, max_frame) {
            chunk.bytes.truncate(start);
            frames.push(chunk.seal(max_frame)?);
            chunk = SnapshotChunk::new(tag);
            append(&mut chunk.bytes, item)?;
        }
        chunk.count += 1;
        chunk.values += 1;
    }
    if !chunk.is_empty() {
        frames.push(chunk.seal(max_frame)?);
    }
    Ok(frames)
}

/// Appends the postcard encoding of `item` to `bytes`.
fn append_encoded<T: Serialize>(bytes: &mut Vec<u8>, item: &T) -> Result<(), NetError> {
    let buffer = std::mem::take(bytes);
    *bytes = postcard::to_extend(item, buffer)
        .map_err(|err| NetError::Protocol(format!("cannot encode message: {err}")))?;
    Ok(())
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
        self.fits(self.bytes.len() + entry_len, values, max_frame)
    }

    /// Whether the chunk, holding `bytes_len` bytes of entries in total, with
    /// one more entry counted in the header, fits the frame and the budget.
    /// `values` decoded values are added to the ones already counted.
    fn fits(&self, bytes_len: usize, values: usize, max_frame: usize) -> bool {
        let payload = varint_len(u64::from(self.tag))
            + varint_len(u64::try_from(self.count + 1).unwrap_or(u64::MAX))
            + bytes_len;
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
        | Value::Vector2(_)
        | Value::Vector3(_)
        | Value::Vector4(_)
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

/// Encodes the body of a latent event: its name and its arguments, in the same
/// form as an event. The body is sent on a stream of its own, so it may be
/// larger than a frame, up to [`MAX_LATENT_EVENT_LEN`] bytes.
pub(crate) fn encode_latent_body(name: &str, args: &[Value]) -> Result<Vec<u8>, NetError> {
    validate_event_name(name)?;
    check_values(args)?;
    if args.iter().map(count_values).sum::<usize>() > MAX_DECODED_VALUES {
        return Err(NetError::InvalidInput(format!(
            "a latent event may contain at most {MAX_DECODED_VALUES} values"
        )));
    }
    let event = (name, Elements(args));
    let len = postcard::experimental::serialized_size(&event)
        .map_err(|err| NetError::Protocol(format!("cannot size latent event: {err}")))?;
    if len > MAX_LATENT_EVENT_LEN {
        return Err(NetError::FrameTooLarge {
            len,
            max: MAX_LATENT_EVENT_LEN,
        });
    }
    let mut body = Vec::with_capacity(len);
    append_encoded(&mut body, &event)?;
    Ok(body)
}

/// Decodes the body of a latent event, which must hold exactly one event under
/// the same limits as a frame.
pub(crate) fn decode_latent_body(payload: &[u8]) -> Result<(String, Args), NetError> {
    let budget = ValueBudget::new();
    let (name, args) = decode_budgeted(payload, &budget, EventSeed { budget: &budget })?;
    validate_event_name(&name)?;
    Ok((name, args))
}

/// Encodes a progress message for a connection that is held by deferrals.
/// A message longer than [`MAX_DEFERRAL_MESSAGE_LEN`] bytes is cut at the
/// last character boundary within the limit.
pub(crate) fn encode_deferral(message: &str, max_frame: usize) -> Result<Arc<[u8]>, NetError> {
    let mut end = message.len().min(MAX_DEFERRAL_MESSAGE_LEN);
    while !message.is_char_boundary(end) {
        end -= 1;
    }
    encode_frame(
        &DeferralFrame {
            message: &message[..end],
        },
        max_frame,
    )
}

/// Borrowed encoding of a deferral message, written with its wire tag.
struct DeferralFrame<'a> {
    message: &'a str,
}

impl Serialize for DeferralFrame<'_> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_newtype_variant(
            MESSAGE_NAME,
            ServerMessage::DEFERRAL_TAG,
            "Deferral",
            self.message,
        )
    }
}

/// Encodes a console line a client sends to the server. The line must be 1 to
/// [`MAX_COMMAND_LEN`] bytes long.
pub(crate) fn encode_command(line: &str, max_frame: usize) -> Result<Arc<[u8]>, NetError> {
    validate_length("command line", line, MAX_COMMAND_LEN)?;
    encode_frame(
        &ClientMessage::Command {
            line: line.to_owned(),
        },
        max_frame,
    )
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
            Value::Vector2(v) => {
                serializer.serialize_newtype_variant(VALUE_NAME, TAG_VECTOR2, "Vector2", v)
            }
            Value::Vector3(v) => {
                serializer.serialize_newtype_variant(VALUE_NAME, TAG_VECTOR3, "Vector3", v)
            }
            Value::Vector4(v) => {
                serializer.serialize_newtype_variant(VALUE_NAME, TAG_VECTOR4, "Vector4", v)
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
            ClientMessage::COMMAND_TAG => {
                let line: String = variant.newtype_variant()?;
                if line.len() > MAX_COMMAND_LEN {
                    return Err(de::Error::custom(format!(
                        "command line is over {MAX_COMMAND_LEN} bytes"
                    )));
                }
                Ok(ClientMessage::Command { line })
            }
            ClientMessage::ENTITY_TRANSFORMS_TAG => variant
                .newtype_variant_seed(TransformsSeed::new(self.budget, MAX_TRANSFORMS_PER_MESSAGE))
                .map(|updates| ClientMessage::EntityTransforms { updates }),
            ClientMessage::CREATE_ENTITY_TAG => variant
                .newtype_variant::<(u32, u8, u32, [f32; 3], [f32; 3])>()
                .map(
                    |(id, kind, model, position, rotation)| ClientMessage::CreateEntity {
                        id,
                        kind,
                        model,
                        position,
                        rotation,
                    },
                ),
            ClientMessage::REQUEST_ENTITY_CONTROL_TAG => variant
                .newtype_variant::<u32>()
                .map(|id| ClientMessage::RequestEntityControl { id }),
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
            ServerMessage::WELCOME_TAG
            | ServerMessage::REJECTED_TAG
            | ServerMessage::EVENT_TAG
            | ServerMessage::KICKED_TAG
            | ServerMessage::CONVARS_TAG
            | ServerMessage::DEFERRAL_TAG => self.visit_session_variant(tag, variant),
            ServerMessage::STATE_BAG_TAG
            | ServerMessage::STATE_BAG_REMOVED_TAG
            | ServerMessage::STATE_SNAPSHOT_TAG
            | ServerMessage::RESOURCES_TAG
            | ServerMessage::RESOURCE_STARTED_TAG
            | ServerMessage::RESOURCE_STOPPED_TAG
            | ServerMessage::FILE_UNAVAILABLE_TAG => self.visit_resource_variant(tag, variant),
            ServerMessage::ENTITY_CREATED_TAG
            | ServerMessage::ENTITY_TRANSFORMS_TAG
            | ServerMessage::ENTITY_DELETED_TAG
            | ServerMessage::ENTITY_SNAPSHOT_TAG
            | ServerMessage::ENTITY_ID_LEASE_TAG
            | ServerMessage::ENTITY_CREATE_REFUSED_TAG
            | ServerMessage::ENTITY_OWNER_TAG => self.visit_entity_variant(tag, variant),
            other => Err(de::Error::unknown_variant(
                &other.to_string(),
                SERVER_VARIANTS,
            )),
        }
    }
}

impl<'de> ServerMessageVisitor<'_> {
    fn visit_session_variant<V: VariantAccess<'de>>(
        self,
        tag: u32,
        variant: V,
    ) -> Result<ServerMessage, V::Error> {
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
            ServerMessage::CONVARS_TAG => variant
                .newtype_variant_seed(ConvarsSeed {
                    budget: self.budget,
                })
                .map(|entries| ServerMessage::Convars { entries }),
            ServerMessage::DEFERRAL_TAG => variant
                .newtype_variant::<String>()
                .and_then(checked_deferral::<V::Error>)
                .map(|message| ServerMessage::Deferral { message }),
            _ => unreachable!(),
        }
    }

    fn visit_resource_variant<V: VariantAccess<'de>>(
        self,
        tag: u32,
        variant: V,
    ) -> Result<ServerMessage, V::Error> {
        match tag {
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
                .and_then(|name| checked_name::<V::Error>(name, MAX_RESOURCE_NAME_LEN))
                .map(|name| ServerMessage::ResourceStopped { name }),
            ServerMessage::FILE_UNAVAILABLE_TAG => variant
                .newtype_variant::<FileHash>()
                .map(|hash| ServerMessage::FileUnavailable { hash }),
            _ => unreachable!(),
        }
    }

    fn visit_entity_variant<V: VariantAccess<'de>>(
        self,
        tag: u32,
        variant: V,
    ) -> Result<ServerMessage, V::Error> {
        match tag {
            ServerMessage::ENTITY_CREATED_TAG => variant
                .newtype_variant_seed(EntitySeed {
                    budget: self.budget,
                })
                .map(|entity| ServerMessage::EntityCreated { entity }),
            ServerMessage::ENTITY_TRANSFORMS_TAG => variant
                .newtype_variant_seed(TransformsSeed::new(self.budget, usize::MAX))
                .map(|updates| ServerMessage::EntityTransforms { updates }),
            ServerMessage::ENTITY_DELETED_TAG => variant
                .newtype_variant::<u32>()
                .map(|id| ServerMessage::EntityDeleted { id }),
            ServerMessage::ENTITY_SNAPSHOT_TAG => variant
                .newtype_variant_seed(EntitiesSeed {
                    budget: self.budget,
                })
                .map(|entities| ServerMessage::EntitySnapshot { entities }),
            ServerMessage::ENTITY_ID_LEASE_TAG => variant
                .newtype_variant::<(u32, u32)>()
                .map(|(first, count)| ServerMessage::EntityIdLease { first, count }),
            ServerMessage::ENTITY_CREATE_REFUSED_TAG => variant
                .newtype_variant::<(u32, String)>()
                .and_then(|(id, reason)| {
                    checked_reason::<V::Error>(reason)
                        .map(|reason| ServerMessage::EntityCreateRefused { id, reason })
                }),
            ServerMessage::ENTITY_OWNER_TAG => variant
                .newtype_variant::<(u32, u32, u32, [f32; 3], [f32; 3])>()
                .map(
                    |(id, owner, epoch, position, rotation)| ServerMessage::EntityOwner {
                        id,
                        owner,
                        epoch,
                        position,
                        rotation,
                    },
                ),
            _ => unreachable!(),
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

/// Decodes one entity, which costs one unit of the budget.
#[derive(Clone, Copy)]
struct EntitySeed<'a> {
    budget: &'a ValueBudget,
}

impl<'de> DeserializeSeed<'de> for EntitySeed<'_> {
    type Value = EntityEntry;

    fn deserialize<D: Deserializer<'de>>(self, deserializer: D) -> Result<EntityEntry, D::Error> {
        self.budget.spend::<D::Error>()?;
        let entry = EntityEntry::deserialize(deserializer)?;
        checked_entity_kind::<D::Error>(&entry)?;
        Ok(entry)
    }
}

/// Decodes a sequence of entity transforms. Each update costs one unit of the
/// budget, and a sequence longer than `limit` is rejected.
struct TransformsSeed<'a> {
    budget: &'a ValueBudget,
    limit: usize,
}

impl<'a> TransformsSeed<'a> {
    fn new(budget: &'a ValueBudget, limit: usize) -> Self {
        Self { budget, limit }
    }
}

impl<'de> DeserializeSeed<'de> for TransformsSeed<'_> {
    type Value = Vec<EntityTransform>;

    fn deserialize<D: Deserializer<'de>>(self, deserializer: D) -> Result<Self::Value, D::Error> {
        deserializer.deserialize_seq(TransformsVisitor {
            budget: self.budget,
            limit: self.limit,
        })
    }
}

struct TransformsVisitor<'a> {
    budget: &'a ValueBudget,
    limit: usize,
}

impl<'de> Visitor<'de> for TransformsVisitor<'_> {
    type Value = Vec<EntityTransform>;

    fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
        formatter.write_str("a sequence of entity transforms")
    }

    fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Self::Value, A::Error> {
        let mut updates = Vec::new();
        while let Some(update) = seq.next_element_seed(TransformSeed)? {
            self.budget.spend::<A::Error>()?;
            if updates.len() == self.limit {
                return Err(de::Error::custom(format!(
                    "a message carries at most {} entity transforms",
                    self.limit
                )));
            }
            updates.push(update);
        }
        Ok(updates)
    }
}

/// Decodes one entity transform, the inverse of `append_transform`. The
/// position and rotation are read only when the flags announce them.
#[derive(Clone, Copy)]
struct TransformSeed;

impl<'de> DeserializeSeed<'de> for TransformSeed {
    type Value = EntityTransform;

    fn deserialize<D: Deserializer<'de>>(self, deserializer: D) -> Result<Self::Value, D::Error> {
        // The tuple length is an upper bound: postcard reads elements in
        // order, and an absent position or rotation is simply not read.
        deserializer.deserialize_tuple(5, TransformVisitor)
    }
}

struct TransformVisitor;

impl<'de> Visitor<'de> for TransformVisitor {
    type Value = EntityTransform;

    fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
        formatter.write_str("an entity transform")
    }

    fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Self::Value, A::Error> {
        let id: u32 = seq
            .next_element()?
            .ok_or_else(|| de::Error::invalid_length(0, &"an entity id"))?;
        let flags: u8 = seq
            .next_element()?
            .ok_or_else(|| de::Error::invalid_length(1, &"transform flags"))?;
        if flags & !TRANSFORM_FLAGS != 0 || flags & TRANSFORM_FLAGS == 0 {
            return Err(de::Error::custom(format!(
                "entity transform flags {flags:#04x} must set bit 0 or bit 1 and no other bit"
            )));
        }
        let position = if flags & TRANSFORM_POSITION != 0 {
            let position = seq
                .next_element_seed(PositionSeed)?
                .ok_or_else(|| de::Error::invalid_length(2, &"a position"))?;
            Some(position)
        } else {
            None
        };
        let rotation = if flags & TRANSFORM_ROTATION != 0 {
            let rotation = seq
                .next_element_seed(RotationSeed)?
                .ok_or_else(|| de::Error::invalid_length(3, &"a rotation"))?;
            Some(rotation)
        } else {
            None
        };
        let epoch: u32 = seq
            .next_element()?
            .ok_or_else(|| de::Error::invalid_length(4, &"an epoch"))?;
        Ok((id, position, rotation, epoch))
    }
}

/// Decodes a position: three zigzag varints of centimetres.
#[derive(Clone, Copy)]
struct PositionSeed;

impl<'de> DeserializeSeed<'de> for PositionSeed {
    type Value = [f32; 3];

    fn deserialize<D: Deserializer<'de>>(self, deserializer: D) -> Result<[f32; 3], D::Error> {
        deserializer.deserialize_tuple(3, PositionVisitor)
    }
}

struct PositionVisitor;

impl<'de> Visitor<'de> for PositionVisitor {
    type Value = [f32; 3];

    fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
        formatter.write_str("three coordinates in centimetres")
    }

    fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<[f32; 3], A::Error> {
        let mut position = [0.0_f32; 3];
        for (index, component) in position.iter_mut().enumerate() {
            let centimetres: i32 = seq
                .next_element()?
                .ok_or_else(|| de::Error::invalid_length(index, &"three coordinates"))?;
            *component = metres(centimetres);
        }
        Ok(position)
    }
}

/// Decodes a rotation: three `u16` little-endian angle counts, six raw bytes.
#[derive(Clone, Copy)]
struct RotationSeed;

impl<'de> DeserializeSeed<'de> for RotationSeed {
    type Value = [f32; 3];

    fn deserialize<D: Deserializer<'de>>(self, deserializer: D) -> Result<[f32; 3], D::Error> {
        deserializer.deserialize_tuple(6, RotationVisitor)
    }
}

struct RotationVisitor;

impl<'de> Visitor<'de> for RotationVisitor {
    type Value = [f32; 3];

    fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
        formatter.write_str("three rotation angles")
    }

    fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<[f32; 3], A::Error> {
        let mut rotation = [0.0_f32; 3];
        for (index, component) in rotation.iter_mut().enumerate() {
            let low: u8 = seq
                .next_element()?
                .ok_or_else(|| de::Error::invalid_length(2 * index, &"three rotation angles"))?;
            let high: u8 = seq.next_element()?.ok_or_else(|| {
                de::Error::invalid_length(2 * index + 1, &"three rotation angles")
            })?;
            *component = degrees(u16::from_le_bytes([low, high]));
        }
        Ok(rotation)
    }
}

/// Decodes the entries of a `Convars` message. Each entry costs one unit of the
/// budget, and its name and value must fit the convar limits.
#[derive(Clone, Copy)]
struct ConvarsSeed<'a> {
    budget: &'a ValueBudget,
}

impl<'de> DeserializeSeed<'de> for ConvarsSeed<'_> {
    type Value = Vec<(String, Option<String>)>;

    fn deserialize<D: Deserializer<'de>>(
        self,
        deserializer: D,
    ) -> Result<Vec<(String, Option<String>)>, D::Error> {
        deserializer.deserialize_seq(ConvarsVisitor {
            budget: self.budget,
        })
    }
}

struct ConvarsVisitor<'a> {
    budget: &'a ValueBudget,
}

impl<'de> Visitor<'de> for ConvarsVisitor<'_> {
    type Value = Vec<(String, Option<String>)>;

    fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
        formatter.write_str("a sequence of convars")
    }

    fn visit_seq<A: SeqAccess<'de>>(
        self,
        mut seq: A,
    ) -> Result<Vec<(String, Option<String>)>, A::Error> {
        let mut entries = Vec::new();
        while let Some((name, value)) = seq.next_element::<(String, Option<String>)>()? {
            self.budget.spend::<A::Error>()?;
            let name = checked_name::<A::Error>(name, MAX_CONVAR_NAME_LEN)?;
            if let Some(value) = &value
                && value.len() > MAX_CONVAR_VALUE_LEN
            {
                return Err(de::Error::custom(format!(
                    "convar value must be at most {MAX_CONVAR_VALUE_LEN} bytes"
                )));
            }
            entries.push((name, value));
        }
        Ok(entries)
    }
}

/// Decodes a sequence of entities. Each entity costs one unit of the budget.
#[derive(Clone, Copy)]
struct EntitiesSeed<'a> {
    budget: &'a ValueBudget,
}

impl<'de> DeserializeSeed<'de> for EntitiesSeed<'_> {
    type Value = Vec<EntityEntry>;

    fn deserialize<D: Deserializer<'de>>(
        self,
        deserializer: D,
    ) -> Result<Vec<EntityEntry>, D::Error> {
        deserializer.deserialize_seq(EntitiesVisitor {
            budget: self.budget,
        })
    }
}

struct EntitiesVisitor<'a> {
    budget: &'a ValueBudget,
}

impl<'de> Visitor<'de> for EntitiesVisitor<'_> {
    type Value = Vec<EntityEntry>;

    fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
        formatter.write_str("a sequence of entities")
    }

    fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Vec<EntityEntry>, A::Error> {
        let mut entities = Vec::new();
        while let Some(entry) = seq.next_element::<EntityEntry>()? {
            self.budget.spend::<A::Error>()?;
            checked_entity_kind::<A::Error>(&entry)?;
            entities.push(entry);
        }
        Ok(entities)
    }
}

/// Rejects an entity whose kind code is not one of the known kinds.
fn checked_entity_kind<E: de::Error>(entry: &EntityEntry) -> Result<(), E> {
    if EntityKind::from_code(entry.kind).is_some() {
        Ok(())
    } else {
        Err(E::custom(format!("unknown entity kind {}", entry.kind)))
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

/// Decodes one resource entry, `(name, files, exports, metadata)`. The entry
/// costs one unit of the budget, and each file, export and metadata pair one more.
#[derive(Clone, Copy)]
struct ResourceEntrySeed<'a> {
    budget: &'a ValueBudget,
}

impl<'de> DeserializeSeed<'de> for ResourceEntrySeed<'_> {
    type Value = ResourceEntry;

    fn deserialize<D: Deserializer<'de>>(self, deserializer: D) -> Result<ResourceEntry, D::Error> {
        self.budget.spend::<D::Error>()?;
        deserializer.deserialize_tuple(
            4,
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
        formatter.write_str("a resource name, its files, its exports and its metadata")
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
        let exports = seq
            .next_element_seed(ExportsSeed {
                budget: self.budget,
            })?
            .ok_or_else(|| de::Error::invalid_length(2, &"a list of exports"))?;
        let metadata = seq
            .next_element_seed(MetadataSeed {
                budget: self.budget,
            })?
            .ok_or_else(|| de::Error::invalid_length(3, &"a list of metadata pairs"))?;
        Ok(ResourceEntry {
            name,
            files,
            exports,
            metadata,
        })
    }
}

/// Decodes a list of metadata pairs, each of which costs one unit of the budget.
#[derive(Clone, Copy)]
struct MetadataSeed<'a> {
    budget: &'a ValueBudget,
}

impl<'de> DeserializeSeed<'de> for MetadataSeed<'_> {
    type Value = Vec<(String, String)>;

    fn deserialize<D: Deserializer<'de>>(
        self,
        deserializer: D,
    ) -> Result<Vec<(String, String)>, D::Error> {
        deserializer.deserialize_seq(MetadataVisitor {
            budget: self.budget,
        })
    }
}

struct MetadataVisitor<'a> {
    budget: &'a ValueBudget,
}

impl<'de> Visitor<'de> for MetadataVisitor<'_> {
    type Value = Vec<(String, String)>;

    fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
        formatter.write_str("a list of metadata pairs")
    }

    fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Vec<(String, String)>, A::Error> {
        let mut metadata = Vec::new();
        while let Some((key, value)) = seq.next_element::<(String, String)>()? {
            self.budget.spend::<A::Error>()?;
            if metadata.len() == MAX_METADATA {
                return Err(de::Error::custom(format!(
                    "a resource declares more than {MAX_METADATA} metadata pairs"
                )));
            }
            let key = checked_name::<A::Error>(key, MAX_METADATA_KEY_LEN)?;
            if value.len() > MAX_METADATA_VALUE_LEN {
                return Err(de::Error::custom(format!(
                    "a metadata value is longer than {MAX_METADATA_VALUE_LEN} bytes"
                )));
            }
            metadata.push((key, value));
        }
        Ok(metadata)
    }
}

/// Decodes a list of export names, each of which costs one unit of the budget.
#[derive(Clone, Copy)]
struct ExportsSeed<'a> {
    budget: &'a ValueBudget,
}

impl<'de> DeserializeSeed<'de> for ExportsSeed<'_> {
    type Value = Vec<String>;

    fn deserialize<D: Deserializer<'de>>(self, deserializer: D) -> Result<Vec<String>, D::Error> {
        deserializer.deserialize_seq(ExportsVisitor {
            budget: self.budget,
        })
    }
}

struct ExportsVisitor<'a> {
    budget: &'a ValueBudget,
}

impl<'de> Visitor<'de> for ExportsVisitor<'_> {
    type Value = Vec<String>;

    fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
        formatter.write_str("a list of export names")
    }

    fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Vec<String>, A::Error> {
        let mut exports = Vec::new();
        while let Some(name) = seq.next_element::<String>()? {
            self.budget.spend::<A::Error>()?;
            if exports.len() == MAX_EXPORTS {
                return Err(de::Error::custom(format!(
                    "a resource declares more than {MAX_EXPORTS} exports"
                )));
            }
            exports.push(checked_name::<A::Error>(name, MAX_EXPORT_NAME_LEN)?);
        }
        Ok(exports)
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

/// Checks a decoded deferral message against its limit, as the encoder does.
fn checked_deferral<E: de::Error>(message: String) -> Result<String, E> {
    if message.len() > MAX_DEFERRAL_MESSAGE_LEN {
        Err(E::custom(format!(
            "deferral message is over {MAX_DEFERRAL_MESSAGE_LEN} bytes"
        )))
    } else {
        Ok(message)
    }
}

/// Checks a decoded refusal reason against its limit.
fn checked_reason<E: de::Error>(reason: String) -> Result<String, E> {
    if reason.len() > MAX_REFUSAL_REASON_LEN {
        Err(E::custom(format!(
            "refusal reason is over {MAX_REFUSAL_REASON_LEN} bytes"
        )))
    } else {
        Ok(reason)
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
            TAG_VECTOR2 => variant.newtype_variant().map(Value::Vector2),
            TAG_VECTOR3 => variant.newtype_variant().map(Value::Vector3),
            TAG_VECTOR4 => variant.newtype_variant().map(Value::Vector4),
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
    fn latent_sender_checks_size_and_nested_value_limits() {
        let oversized = Value::String(vec![0; MAX_LATENT_EVENT_LEN].into_boxed_slice());
        assert!(matches!(
            encode_latent_body("x", &[oversized]),
            Err(NetError::FrameTooLarge { .. })
        ));
        let array = Value::Array(vec![Value::Nil; MAX_DECODED_VALUES]);
        assert!(matches!(
            encode_latent_body("x", &[array]),
            Err(NetError::InvalidInput(_))
        ));
        let body = encode_latent_body("x", &[Value::Nil]).expect("encodes");
        let (name, args) = decode_latent_body(&body).expect("decodes");
        assert_eq!(name, "x");
        assert_eq!(args.0, vec![Value::Nil]);
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
    fn vectors_round_trip_through_an_event_payload() {
        let nan = f32::from_bits(0x7FC0_0001);
        let args = vec![
            Value::Vector2([1.5, nan]),
            Value::Vector3([-0.0, f32::INFINITY, 3.0]),
            Value::Vector4([nan, 2.0, -4.25, f32::NEG_INFINITY]),
        ];
        let payload = payload_of(&event_with(args));
        let Ok(ClientMessage::Event { args, .. }) = decode_client_message(&payload) else {
            panic!("event with vectors must decode");
        };
        let [Value::Vector2(v2), Value::Vector3(v3), Value::Vector4(v4)] = &args.0[..] else {
            panic!("vectors must decode as vectors, got {:?}", args.0);
        };
        assert_eq!(v2.map(f32::to_bits), [1.5_f32.to_bits(), nan.to_bits()]);
        assert_eq!(
            v3.map(f32::to_bits),
            [
                (-0.0_f32).to_bits(),
                f32::INFINITY.to_bits(),
                3.0_f32.to_bits()
            ]
        );
        assert_eq!(
            v4.map(f32::to_bits),
            [
                nan.to_bits(),
                2.0_f32.to_bits(),
                (-4.25_f32).to_bits(),
                f32::NEG_INFINITY.to_bits()
            ]
        );
    }

    #[test]
    fn truncated_vector_payload_is_rejected() {
        let payload = payload_of(&event_with(vec![Value::Vector3([1.0, 2.0, 3.0])]));
        for cut in 1..=12 {
            let short = &payload[..payload.len() - cut];
            assert!(
                matches!(decode_client_message(short), Err(NetError::Protocol(_))),
                "payload cut by {cut} bytes must be rejected"
            );
        }
    }

    #[test]
    fn vectors_cost_one_value_each_in_the_budget() {
        let args = vec![Value::Vector2([0.0; 2]); MAX_DECODED_VALUES];
        let payload = payload_of(&event_with(args));
        assert!(decode_client_message(&payload).is_ok());
        let args = vec![Value::Vector2([0.0; 2]); MAX_DECODED_VALUES + 1];
        let payload = payload_of(&event_with(args));
        assert!(matches!(
            decode_client_message(&payload),
            Err(NetError::Protocol(reason)) if reason.contains("values")
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
    fn command_round_trips() {
        let command = ClientMessage::Command {
            line: "hello world".to_owned(),
        };
        assert!(matches!(
            decode_client_message(&payload_of(&command)),
            Ok(ClientMessage::Command { line }) if line == "hello world"
        ));
    }

    #[test]
    fn command_line_is_limited_to_the_maximum_length() {
        let longest = "c".repeat(MAX_COMMAND_LEN);
        let frame =
            encode_command(&longest, MAX_FRAME).expect("a line of the maximum length encodes");
        assert!(matches!(
            decode_client_message(&frame[FRAME_HEADER_LEN..]),
            Ok(ClientMessage::Command { line }) if line.len() == MAX_COMMAND_LEN
        ));
        assert!(encode_command(&"c".repeat(MAX_COMMAND_LEN + 1), MAX_FRAME).is_err());
        assert!(encode_command("", MAX_FRAME).is_err());

        let too_long = ClientMessage::Command {
            line: "c".repeat(MAX_COMMAND_LEN + 1),
        };
        assert!(decode_client_message(&payload_of(&too_long)).is_err());
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
            exports: Vec::new(),
            metadata: Vec::new(),
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
            exports: Vec::new(),
            metadata: Vec::new(),
        };
        let payload = payload_of(&ServerMessage::ResourceStarted { entry });
        assert!(matches!(
            decode_server_message(&payload),
            Err(NetError::Protocol(_))
        ));
    }

    #[test]
    fn resource_started_with_exports_round_trips() {
        let entry = ResourceEntry {
            name: "provider".to_owned(),
            files: vec![file("client/main.lua", FILE_KIND_CLIENT, 1)],
            exports: vec!["greet".to_owned(), "notDeclared".to_owned()],
            metadata: Vec::new(),
        };
        let payload = payload_of(&ServerMessage::ResourceStarted {
            entry: entry.clone(),
        });
        assert!(matches!(
            decode_server_message(&payload),
            Ok(ServerMessage::ResourceStarted { entry: got }) if got == entry
        ));
    }

    #[test]
    fn export_with_an_overlong_name_is_rejected() {
        let entry = ResourceEntry {
            name: "provider".to_owned(),
            files: Vec::new(),
            exports: vec!["x".repeat(MAX_EXPORT_NAME_LEN + 1)],
            metadata: Vec::new(),
        };
        let payload = payload_of(&ServerMessage::ResourceStarted { entry });
        assert!(matches!(
            decode_server_message(&payload),
            Err(NetError::Protocol(_))
        ));
    }

    #[test]
    fn too_many_exports_are_rejected() {
        let entry = ResourceEntry {
            name: "provider".to_owned(),
            files: Vec::new(),
            exports: (0..=MAX_EXPORTS).map(|i| format!("export{i}")).collect(),
            metadata: Vec::new(),
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
                    Some(last) if last.name == entry.name => {
                        last.files.extend(entry.files);
                        last.exports.extend(entry.exports);
                        last.metadata.extend(entry.metadata);
                    }
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
                exports: Vec::new(),
                metadata: Vec::new(),
            },
            ResourceEntry {
                name: "big".to_owned(),
                files: many,
                exports: Vec::new(),
                metadata: Vec::new(),
            },
            ResourceEntry {
                name: "after".to_owned(),
                files: vec![file("a.lua", FILE_KIND_SHARED, 3)],
                exports: Vec::new(),
                metadata: Vec::new(),
            },
        ];
        let frames = encode_resources_snapshot(&resources, MAX).expect("snapshot encodes");
        assert!(frames.len() > 2, "the snapshot must span several frames");
        assert_eq!(decode_resources_snapshot(&frames, MAX), resources);
    }

    #[test]
    fn snapshot_carries_the_exports_once_per_resource() {
        const MAX: usize = 16 * 1024;
        let many: Vec<FileEntry> = (0..600)
            .map(|i| {
                let byte = u8::try_from(i % 200).expect("fits a byte");
                file(&format!("file{i}.lua"), FILE_KIND_FILE, byte)
            })
            .collect();
        let resources = vec![ResourceEntry {
            name: "provider".to_owned(),
            files: many,
            exports: vec!["greet".to_owned(), "bye".to_owned(), "sum".to_owned()],
            metadata: Vec::new(),
        }];
        let frames = encode_resources_snapshot(&resources, MAX).expect("snapshot encodes");
        assert!(frames.len() > 2, "the snapshot must span several frames");
        assert_eq!(decode_resources_snapshot(&frames, MAX), resources);
    }

    #[test]
    fn resource_started_with_metadata_round_trips() {
        let entry = ResourceEntry {
            name: "provider".to_owned(),
            files: Vec::new(),
            exports: Vec::new(),
            metadata: vec![
                ("version".to_owned(), "1.2.0".to_owned()),
                ("author".to_owned(), String::new()),
            ],
        };
        let payload = payload_of(&ServerMessage::ResourceStarted {
            entry: entry.clone(),
        });
        assert!(matches!(
            decode_server_message(&payload),
            Ok(ServerMessage::ResourceStarted { entry: got }) if got == entry
        ));
    }

    #[test]
    fn metadata_over_its_limits_is_rejected() {
        let entries = [
            ResourceEntry {
                name: "meta".to_owned(),
                files: Vec::new(),
                exports: Vec::new(),
                metadata: vec![("k".repeat(MAX_METADATA_KEY_LEN + 1), "v".to_owned())],
            },
            ResourceEntry {
                name: "meta".to_owned(),
                files: Vec::new(),
                exports: Vec::new(),
                metadata: vec![("k".to_owned(), "v".repeat(MAX_METADATA_VALUE_LEN + 1))],
            },
            ResourceEntry {
                name: "meta".to_owned(),
                files: Vec::new(),
                exports: Vec::new(),
                metadata: (0..=MAX_METADATA)
                    .map(|i| (format!("key{i}"), String::new()))
                    .collect(),
            },
        ];
        for entry in entries {
            let payload = payload_of(&ServerMessage::ResourceStarted { entry });
            assert!(matches!(
                decode_server_message(&payload),
                Err(NetError::Protocol(_))
            ));
        }
    }

    #[test]
    fn snapshot_carries_the_metadata_once_per_resource() {
        const MAX: usize = 16 * 1024;
        let many: Vec<FileEntry> = (0..600)
            .map(|i| {
                let byte = u8::try_from(i % 200).expect("fits a byte");
                file(&format!("file{i}.lua"), FILE_KIND_FILE, byte)
            })
            .collect();
        let resources = vec![ResourceEntry {
            name: "provider".to_owned(),
            files: many,
            exports: Vec::new(),
            metadata: vec![
                ("version".to_owned(), "1.2.0".to_owned()),
                ("author".to_owned(), "someone".to_owned()),
            ],
        }];
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
                exports: Vec::new(),
                metadata: Vec::new(),
            })
            .collect();
        let frames = encode_resources_snapshot(&resources, MAX_FRAME).expect("snapshot encodes");
        assert_eq!(decode_resources_snapshot(&frames, MAX_FRAME), resources);
    }

    /// One entity transform, as the server sends it.
    type Update = EntityTransform;

    /// A sequence message's payload with `updates` written by the transform
    /// encoder, without the budget or frame checks, so that a test can build a
    /// message the encoder itself refuses to split.
    fn raw_transforms(tag: u32, updates: &[Update]) -> Vec<u8> {
        let mut payload =
            postcard::to_extend(&(tag, updates.len()), Vec::new()).expect("header encodes");
        for update in updates {
            append_transform(&mut payload, update).expect("update encodes");
        }
        payload
    }

    /// The angle between two rotation angles, in degrees, taking the wrap at 360 into account.
    fn angle_gap(a: f32, b: f32) -> f32 {
        let difference = (a - b).abs() % 360.0;
        difference.min(360.0 - difference)
    }

    fn entry(id: u32) -> EntityEntry {
        EntityEntry {
            id,
            kind: 2,
            model: 0xB779_A091,
            position: [1.0, 2.0, 3.0],
            rotation: [0.0, 0.0, 90.0],
            owner: 0,
            epoch: 0,
        }
    }

    /// Decodes every frame of a sequence message, checking each frame's length
    /// prefix and size, and returns the items `take` extracts, in order.
    fn decode_frames<T>(
        frames: &[Arc<[u8]>],
        max_frame: usize,
        take: fn(ServerMessage) -> Option<Vec<T>>,
    ) -> Vec<T> {
        let mut items = Vec::new();
        for frame in frames {
            let len = frame.len() - FRAME_HEADER_LEN;
            assert!(len <= max_frame, "frame of {len} bytes exceeds {max_frame}");
            let prefix = u32::try_from(len).expect("frame fits a u32");
            assert_eq!(&frame[..FRAME_HEADER_LEN], &prefix.to_le_bytes());
            match decode_server_message(&frame[FRAME_HEADER_LEN..]) {
                Ok(message) => {
                    let part = take(message).expect("the frame carries the right list");
                    items.extend(part);
                }
                Err(err) => panic!("the frame does not decode: {err}"),
            }
        }
        items
    }

    fn transforms_of(message: ServerMessage) -> Option<Vec<Update>> {
        match message {
            ServerMessage::EntityTransforms { updates } => Some(updates),
            _ => None,
        }
    }

    fn entities_of(message: ServerMessage) -> Option<Vec<EntityEntry>> {
        match message {
            ServerMessage::EntitySnapshot { entities } => Some(entities),
            _ => None,
        }
    }

    #[test]
    fn entity_messages_round_trip() {
        let created = ServerMessage::EntityCreated { entity: entry(1) };
        assert!(matches!(
            decode_server_message(&payload_of(&created)),
            Ok(ServerMessage::EntityCreated { entity }) if entity == entry(1)
        ));
        let deleted = ServerMessage::EntityDeleted { id: 1 };
        assert!(matches!(
            decode_server_message(&payload_of(&deleted)),
            Ok(ServerMessage::EntityDeleted { id: 1 })
        ));
        let snapshot = ServerMessage::EntitySnapshot {
            entities: vec![entry(1), entry(2)],
        };
        assert!(matches!(
            decode_server_message(&payload_of(&snapshot)),
            Ok(ServerMessage::EntitySnapshot { entities }) if entities == [entry(1), entry(2)]
        ));
        let frames =
            encode_entity_transforms(&[(4, Some([1.0; 3]), Some([45.0; 3]), 3)], MAX_FRAME)
                .expect("transforms encode");
        assert_eq!(
            decode_frames(&frames, MAX_FRAME, transforms_of),
            vec![(4, Some([1.0; 3]), Some([45.0; 3]), 3)]
        );
    }

    #[test]
    fn entity_creation_and_ownership_messages_round_trip() {
        let create = ClientMessage::CreateEntity {
            id: 9,
            kind: 2,
            model: 77,
            position: [1.0, 2.0, 3.0],
            rotation: [0.0, 0.0, 90.0],
        };
        let frame = encode_frame(&create, MAX_FRAME).expect("encodes");
        match decode_client_message(&frame[FRAME_HEADER_LEN..]) {
            Ok(ClientMessage::CreateEntity {
                id: 9,
                kind: 2,
                model: 77,
                position,
                rotation,
            }) => assert_eq!((position, rotation), ([1.0, 2.0, 3.0], [0.0, 0.0, 90.0])),
            other => panic!("unexpected decode: {other:?}"),
        }

        let control = ClientMessage::RequestEntityControl { id: 4 };
        let frame = encode_frame(&control, MAX_FRAME).expect("encodes");
        assert!(matches!(
            decode_client_message(&frame[FRAME_HEADER_LEN..]),
            Ok(ClientMessage::RequestEntityControl { id: 4 })
        ));

        let lease = ServerMessage::EntityIdLease {
            first: 100,
            count: 32,
        };
        let frame = encode_frame(&lease, MAX_FRAME).expect("encodes");
        assert!(matches!(
            decode_server_message(&frame[FRAME_HEADER_LEN..]),
            Ok(ServerMessage::EntityIdLease {
                first: 100,
                count: 32
            })
        ));

        let refused = ServerMessage::EntityCreateRefused {
            id: 101,
            reason: "locked".to_owned(),
        };
        let frame = encode_frame(&refused, MAX_FRAME).expect("encodes");
        assert!(matches!(
            decode_server_message(&frame[FRAME_HEADER_LEN..]),
            Ok(ServerMessage::EntityCreateRefused { id: 101, reason }) if reason == "locked"
        ));

        let owner = ServerMessage::EntityOwner {
            id: 5,
            owner: 3,
            epoch: 7,
            position: [4.0, 5.0, 6.0],
            rotation: [0.0; 3],
        };
        let frame = encode_frame(&owner, MAX_FRAME).expect("encodes");
        assert!(matches!(
            decode_server_message(&frame[FRAME_HEADER_LEN..]),
            Ok(ServerMessage::EntityOwner {
                id: 5,
                owner: 3,
                epoch: 7,
                ..
            })
        ));
    }

    #[test]
    fn an_overlong_refusal_reason_is_rejected_when_decoded() {
        let refused = ServerMessage::EntityCreateRefused {
            id: 1,
            reason: "x".repeat(MAX_REFUSAL_REASON_LEN + 1),
        };
        let frame = encode_frame(&refused, MAX_FRAME).expect("encodes");
        assert!(decode_server_message(&frame[FRAME_HEADER_LEN..]).is_err());
    }

    #[test]
    fn client_transforms_round_trip_and_split_at_the_message_limit() {
        let updates: Vec<Update> = (1..=2500_u32)
            .map(|id| (id, Some([1.0, 2.0, 3.0]), Some([0.0, 0.0, 90.0]), 0))
            .collect();
        let frames = encode_client_transforms(&updates, MAX_FRAME).expect("transforms encode");
        assert_eq!(frames.len(), 3, "2500 updates make 1024 + 1024 + 452");
        let mut decoded = Vec::new();
        for frame in &frames {
            match decode_client_message(&frame[FRAME_HEADER_LEN..]) {
                Ok(ClientMessage::EntityTransforms { updates }) => {
                    assert!(updates.len() <= MAX_TRANSFORMS_PER_MESSAGE);
                    decoded.extend(updates);
                }
                other => panic!("unexpected decode: {other:?}"),
            }
        }
        assert_eq!(decoded, updates);
        assert_eq!(
            encode_client_transforms(&[], MAX_FRAME)
                .expect("empty")
                .len(),
            0
        );
    }

    #[test]
    fn client_transform_messages_over_the_limit_are_rejected() {
        let over = vec![(1, Some([0.0; 3]), None, 0); MAX_TRANSFORMS_PER_MESSAGE + 1];
        let payload = raw_transforms(ClientMessage::ENTITY_TRANSFORMS_TAG, &over);
        assert!(matches!(
            decode_client_message(&payload),
            Err(NetError::Protocol(_))
        ));
    }

    #[test]
    fn empty_entity_lists_send_nothing() {
        assert_eq!(
            encode_entity_transforms(&[], MAX_FRAME)
                .expect("empty batch encodes")
                .len(),
            0
        );
        assert_eq!(
            encode_entity_snapshot(&[], MAX_FRAME)
                .expect("empty snapshot encodes")
                .len(),
            0
        );
    }

    #[test]
    fn unknown_entity_kinds_are_rejected() {
        let bad = EntityEntry {
            kind: 4,
            ..entry(1)
        };
        let created = ServerMessage::EntityCreated { entity: bad };
        assert!(decode_server_message(&payload_of(&created)).is_err());
        let snapshot = ServerMessage::EntitySnapshot {
            entities: vec![entry(1), bad],
        };
        assert!(decode_server_message(&payload_of(&snapshot)).is_err());
    }

    #[test]
    fn entity_transforms_split_and_decode_to_the_same_updates() {
        const MAX: usize = 512;
        let updates: Vec<Update> = (0..3000_u32)
            .map(|id| match id % 3 {
                0 => (id, Some([1.0, 2.0, 3.0]), Some([0.0, 0.0, 90.0]), id % 7),
                1 => (id, Some([-1.0, 0.0, 250.0]), None, id % 7),
                _ => (id, None, Some([0.0, 180.0, 45.0]), id % 7),
            })
            .collect();
        let frames = encode_entity_transforms(&updates, MAX).expect("transforms encode");
        assert!(
            frames.len() > 1,
            "3000 updates cannot fit one 512-byte frame"
        );
        assert_eq!(decode_frames(&frames, MAX, transforms_of), updates);
    }

    #[test]
    fn transforms_carry_only_the_parts_that_changed() {
        let updates: Vec<Update> = vec![
            (1, Some([1.0, 2.0, 3.0]), None, 4),
            (2, None, Some([0.0, 0.0, 90.0]), 5),
            (3, Some([-4.5, 0.25, 0.0]), Some([45.0, 180.0, 270.0]), 6),
        ];
        let frames = encode_entity_transforms(&updates, MAX_FRAME).expect("transforms encode");
        assert_eq!(decode_frames(&frames, MAX_FRAME, transforms_of), updates);
    }

    #[test]
    fn a_transform_has_the_flags_byte_and_quantized_parts_on_the_wire() {
        // Message tag 4, one update, id 1, flags 0b01 (position only), the
        // position 1 m and -0.5 m as zigzag centimetres 200, 99 and 0, then epoch 0.
        let frames = encode_client_transforms(&[(1, Some([1.0, -0.5, 0.0]), None, 0)], MAX_FRAME)
            .expect("transforms encode");
        assert_eq!(
            frames[0][..],
            [9, 0, 0, 0, 4, 1, 1, 1, 0xC8, 0x01, 0x63, 0x00, 0]
        );
        // Flags 0b10 (rotation only): yaw 90 degrees is 16 384 units, the bytes 0x00 0x40.
        let frames = encode_client_transforms(&[(1, None, Some([0.0, 0.0, 90.0]), 0)], MAX_FRAME)
            .expect("transforms encode");
        assert_eq!(
            frames[0][..],
            [11, 0, 0, 0, 4, 1, 1, 2, 0, 0, 0, 0, 0, 0x40, 0]
        );
    }

    #[test]
    fn a_transform_needs_a_part_and_finite_values_to_be_encoded() {
        assert!(encode_entity_transforms(&[(1, None, None, 0)], MAX_FRAME).is_err());
        assert!(
            encode_entity_transforms(&[(1, Some([f32::NAN, 0.0, 0.0]), None, 0)], MAX_FRAME)
                .is_err()
        );
        assert!(
            encode_entity_transforms(&[(1, None, Some([0.0, f32::INFINITY, 0.0]), 0)], MAX_FRAME)
                .is_err()
        );
    }

    #[test]
    fn transform_flags_with_unknown_bits_or_no_part_are_rejected() {
        for flags in [0_u8, 0b100, 0b1011, 0xFF] {
            let payload = [12, 1, 1, flags, 0, 0, 0, 0, 0, 0, 0, 0];
            assert!(
                matches!(decode_server_message(&payload), Err(NetError::Protocol(_))),
                "flags {flags:#04x} must be refused"
            );
        }
    }

    #[test]
    fn quantization_error_is_at_most_half_a_centimetre_and_one_angle_unit() {
        let positions = [
            [0.004, -0.004, 12.345_678],
            [-987.654_3, 1_234.567_8, -0.499_9],
            [3.0, -0.005_1, 0.0049],
        ];
        let rotations = [
            [0.0001, 359.9999, 123.456_7],
            [-0.0001, 12.3456, 270.0001],
            [0.0027, 179.9999, 90.0055],
        ];
        let unit = 360.0 / 65_536.0;
        for position in positions {
            for rotation in rotations {
                let frames =
                    encode_entity_transforms(&[(1, Some(position), Some(rotation), 0)], MAX_FRAME)
                        .expect("transforms encode");
                let decoded = decode_frames(&frames, MAX_FRAME, transforms_of);
                let (_, decoded_position, decoded_rotation, _) = decoded[0];
                let decoded_position = decoded_position.expect("the position is present");
                let decoded_rotation = decoded_rotation.expect("the rotation is present");
                for axis in 0..3 {
                    assert!(
                        (decoded_position[axis] - position[axis]).abs() <= 0.005 + 1e-4,
                        "position {position:?} decoded as {decoded_position:?}"
                    );
                    assert!(
                        angle_gap(decoded_rotation[axis], rotation[axis]) <= unit + 1e-4,
                        "rotation {rotation:?} decoded as {decoded_rotation:?}"
                    );
                    assert!((0.0..360.0).contains(&decoded_rotation[axis]));
                }
            }
        }
    }

    #[test]
    fn negative_coordinates_and_angles_normalize_correctly() {
        let frames = encode_entity_transforms(
            &[(
                1,
                Some([-12.5, -0.25, -3.0]),
                Some([-90.0, 450.0, -360.0]),
                0,
            )],
            MAX_FRAME,
        )
        .expect("transforms encode");
        let decoded = decode_frames(&frames, MAX_FRAME, transforms_of);
        assert_eq!(
            decoded,
            vec![(1, Some([-12.5, -0.25, -3.0]), Some([270.0, 90.0, 0.0]), 0)]
        );
    }

    #[test]
    fn huge_coordinates_are_clamped_to_the_centimetre_range() {
        let frames =
            encode_entity_transforms(&[(1, Some([1e30, -1e30, f32::MAX]), None, 0)], MAX_FRAME)
                .expect("transforms encode");
        let decoded = decode_frames(&frames, MAX_FRAME, transforms_of);
        let position = decoded[0].1.expect("the position is present");
        // i32::MAX centimetres is 21 474 836.47 m, and i32::MIN is one more below.
        assert!((position[0] - 21_474_836.47).abs() < 0.01, "{position:?}");
        assert!((position[1] + 21_474_836.48).abs() < 0.01, "{position:?}");
        assert!((position[2] - 21_474_836.47).abs() < 0.01, "{position:?}");
    }

    #[test]
    fn entity_snapshot_splits_and_decodes_to_the_same_entities() {
        const MAX: usize = 256;
        let entities: Vec<EntityEntry> = (1..=500_u32).map(entry).collect();
        let frames = encode_entity_snapshot(&entities, MAX).expect("snapshot encodes");
        assert!(
            frames.len() > 1,
            "500 entities cannot fit one 256-byte frame"
        );
        assert_eq!(decode_frames(&frames, MAX, entities_of), entities);
    }

    #[test]
    fn entity_frames_respect_the_decoding_budget() {
        // Each update costs one unit, so 70 000 of them need two frames even
        // though they fit one frame by size.
        const LARGE: usize = 16 * 1024 * 1024;
        let updates = vec![(1_u32, Some([0.0; 3]), None, 0); 70_000];
        let frames = encode_entity_transforms(&updates, LARGE).expect("transforms encode");
        assert_eq!(frames.len(), 2);
        assert_eq!(decode_frames(&frames, LARGE, transforms_of).len(), 70_000);
    }

    #[test]
    fn entity_batches_over_the_decoding_budget_are_rejected() {
        const LARGE: usize = 16 * 1024 * 1024;
        let over = vec![(1, None, Some([0.0; 3]), 0); MAX_DECODED_VALUES + 1];
        let payload = raw_transforms(ServerMessage::ENTITY_TRANSFORMS_TAG, &over);
        assert!(matches!(
            decode_server_message(&payload),
            Err(NetError::Protocol(reason)) if reason.contains("values")
        ));
        let over = ServerMessage::EntitySnapshot {
            entities: vec![entry(1); MAX_DECODED_VALUES + 1],
        };
        let frame = encode_frame(&over, LARGE).expect("message encodes");
        assert!(matches!(
            decode_server_message(&frame[FRAME_HEADER_LEN..]),
            Err(NetError::Protocol(reason)) if reason.contains("values")
        ));
    }

    #[test]
    fn deferral_frames_carry_tag_16_and_decode_to_their_message() {
        let frame = encode_deferral("queue position 3", MAX_FRAME).expect("deferral encodes");
        assert_eq!(frame[FRAME_HEADER_LEN], 16, "the wire tag is 16");
        assert!(matches!(
            decode_server_message(&frame[FRAME_HEADER_LEN..]),
            Ok(ServerMessage::Deferral { message }) if message == "queue position 3"
        ));
    }

    #[test]
    fn over_long_deferral_messages_are_rejected_when_decoded() {
        let long = DeferralFrame {
            message: &"x".repeat(MAX_DEFERRAL_MESSAGE_LEN + 1),
        };
        let frame = encode_frame(&long, MAX_FRAME).expect("the frame itself fits");
        assert!(matches!(
            decode_server_message(&frame[FRAME_HEADER_LEN..]),
            Err(NetError::Protocol(_))
        ));
    }

    #[test]
    fn long_deferral_messages_are_cut_on_a_character_boundary() {
        // The two-byte `é` starts at byte 4095, so the limit of 4096 bytes
        // falls inside it and the message is cut before it.
        let message = format!("{}é", "a".repeat(MAX_DEFERRAL_MESSAGE_LEN - 1));
        let frame = encode_deferral(&message, MAX_FRAME).expect("deferral encodes");
        match decode_server_message(&frame[FRAME_HEADER_LEN..]) {
            Ok(ServerMessage::Deferral { message: cut }) => {
                assert_eq!(cut.len(), MAX_DEFERRAL_MESSAGE_LEN - 1);
                assert!(message.starts_with(&cut));
            }
            other => panic!("expected a deferral, got {other:?}"),
        }
    }

    fn convars_of(message: ServerMessage) -> Option<Vec<(String, Option<String>)>> {
        match message {
            ServerMessage::Convars { entries } => Some(entries),
            _ => None,
        }
    }

    #[test]
    fn convars_round_trip_including_a_removal() {
        let entries = vec![
            ("game_mode".to_owned(), Some("race".to_owned())),
            ("old".to_owned(), None),
            ("empty".to_owned(), Some(String::new())),
        ];
        let message = ServerMessage::Convars {
            entries: entries.clone(),
        };
        match decode_server_message(&payload_of(&message)) {
            Ok(ServerMessage::Convars { entries: got }) => assert_eq!(got, entries),
            other => panic!("the convars message does not round trip: {other:?}"),
        }
    }

    #[test]
    fn convar_limits_are_checked_on_encode_and_decode() {
        let long_name = "n".repeat(MAX_CONVAR_NAME_LEN + 1);
        let long_value = "v".repeat(MAX_CONVAR_VALUE_LEN + 1);
        let exact_value = "v".repeat(MAX_CONVAR_VALUE_LEN);
        assert!(encode_convars(&[("", None)], MAX_FRAME).is_err());
        assert!(encode_convars(&[(long_name.as_str(), None)], MAX_FRAME).is_err());
        assert!(encode_convars(&[("a", Some(long_value.as_str()))], MAX_FRAME).is_err());
        assert!(encode_convars(&[("a", Some(exact_value.as_str()))], MAX_FRAME).is_ok());

        // Decoding applies the same limits to messages that skip the encoder.
        let too_long_name = ServerMessage::Convars {
            entries: vec![(long_name.clone(), None)],
        };
        assert!(decode_server_message(&payload_of(&too_long_name)).is_err());
        let empty_name = ServerMessage::Convars {
            entries: vec![(String::new(), None)],
        };
        assert!(decode_server_message(&payload_of(&empty_name)).is_err());
        let too_long_value = ServerMessage::Convars {
            entries: vec![("a".to_owned(), Some(long_value))],
        };
        assert!(decode_server_message(&payload_of(&too_long_value)).is_err());
    }

    #[test]
    fn convar_snapshot_splits_and_decodes_to_the_same_entries() {
        const MAX: usize = 512;
        let entries: Vec<(String, Option<String>)> = (0..300)
            .map(|index| (format!("convar_{index}"), Some("x".repeat(20))))
            .collect();
        let borrowed: Vec<(&str, Option<&str>)> = entries
            .iter()
            .map(|(name, value)| (name.as_str(), value.as_deref()))
            .collect();
        let frames = encode_convars(&borrowed, MAX).expect("convars encode");
        assert!(
            frames.len() > 1,
            "300 convars cannot fit one 512-byte frame"
        );
        assert_eq!(decode_frames(&frames, MAX, convars_of), entries);
    }

    #[test]
    fn convar_entries_count_against_the_decoding_budget() {
        const LARGE: usize = 16 * 1024 * 1024;
        let entries: Vec<(&str, Option<&str>)> = vec![("a", None); 70_000];
        let frames = encode_convars(&entries, LARGE).expect("convars encode");
        assert_eq!(frames.len(), 2);
        assert_eq!(decode_frames(&frames, LARGE, convars_of).len(), 70_000);

        let over = ServerMessage::Convars {
            entries: vec![("a".to_owned(), None); MAX_DECODED_VALUES + 1],
        };
        assert!(matches!(
            decode_server_message(&payload_of(&over)),
            Err(NetError::Protocol(reason)) if reason.contains("values")
        ));
    }
}
