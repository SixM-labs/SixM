//! Binary encoding of script values and frames.
//!
//! A frame is a `u32` little-endian payload length followed by a postcard
//! payload. Values are encoded as postcard enums, strings as raw bytes and
//! arrays and maps as sequences. Function references have no encoding and
//! cannot be decoded, so a peer can never forge a reference into server code.

use std::fmt;
use std::sync::Arc;

use quinn::{ReadExactError, RecvStream};
use serde::de::{
    self, DeserializeOwned, DeserializeSeed, Deserializer, EnumAccess, SeqAccess, VariantAccess,
    Visitor,
};
use serde::ser::{SerializeSeq, SerializeStructVariant};
use serde::{Deserialize, Serialize, Serializer};
use sixm_core::Value;

use crate::error::NetError;
use crate::protocol::{MAX_EVENT_NAME_LEN, MAX_VALUE_DEPTH};

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

/// Argument list of an event. Decoding enforces [`MAX_VALUE_DEPTH`].
#[derive(Debug, Clone, PartialEq, Default)]
pub(crate) struct Args(pub(crate) Vec<Value>);

impl Serialize for Args {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        Elements(&self.0).serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for Args {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        deserializer
            .deserialize_seq(ValuesVisitor { child_depth: 1 })
            .map(Args)
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
    if name.is_empty() || name.len() > MAX_EVENT_NAME_LEN {
        return Err(NetError::InvalidInput(format!(
            "event name must be 1 to {MAX_EVENT_NAME_LEN} bytes, got {}",
            name.len()
        )));
    }
    Ok(())
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
    let mut buffer = postcard::to_extend(message, vec![0_u8; FRAME_HEADER_LEN])
        .map_err(|err| NetError::Protocol(format!("cannot encode message: {err}")))?;
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

/// Decodes a frame payload, rejecting trailing bytes.
pub(crate) fn decode_payload<T: DeserializeOwned>(payload: &[u8]) -> Result<T, NetError> {
    let (message, rest) = postcard::take_from_bytes::<T>(payload)
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

/// Decodes one value whose nesting depth is `depth`.
#[derive(Clone, Copy)]
struct ValueSeed {
    depth: usize,
}

impl<'de> DeserializeSeed<'de> for ValueSeed {
    type Value = Value;

    fn deserialize<D: Deserializer<'de>>(self, deserializer: D) -> Result<Value, D::Error> {
        if self.depth > MAX_VALUE_DEPTH {
            return Err(de::Error::custom("value nested too deeply"));
        }
        deserializer.deserialize_enum(
            VALUE_NAME,
            VALUE_VARIANTS,
            ValueVisitor { depth: self.depth },
        )
    }
}

struct ValueVisitor {
    depth: usize,
}

impl<'de> Visitor<'de> for ValueVisitor {
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
                .newtype_variant_seed(ValuesSeed { child_depth })
                .map(Value::Array),
            TAG_MAP => variant
                .newtype_variant_seed(PairsSeed { child_depth })
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
struct ValuesSeed {
    child_depth: usize,
}

impl<'de> DeserializeSeed<'de> for ValuesSeed {
    type Value = Vec<Value>;

    fn deserialize<D: Deserializer<'de>>(self, deserializer: D) -> Result<Vec<Value>, D::Error> {
        deserializer.deserialize_seq(ValuesVisitor {
            child_depth: self.child_depth,
        })
    }
}

struct ValuesVisitor {
    child_depth: usize,
}

impl<'de> Visitor<'de> for ValuesVisitor {
    type Value = Vec<Value>;

    fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
        formatter.write_str("a sequence of script values")
    }

    fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Vec<Value>, A::Error> {
        let mut values = Vec::new();
        while let Some(value) = seq.next_element_seed(ValueSeed {
            depth: self.child_depth,
        })? {
            values.push(value);
        }
        Ok(values)
    }
}

/// Decodes a sequence of key/value pairs, each element at depth `child_depth`.
#[derive(Clone, Copy)]
struct PairsSeed {
    child_depth: usize,
}

impl<'de> DeserializeSeed<'de> for PairsSeed {
    type Value = Vec<(Value, Value)>;

    fn deserialize<D: Deserializer<'de>>(
        self,
        deserializer: D,
    ) -> Result<Vec<(Value, Value)>, D::Error> {
        deserializer.deserialize_seq(PairsVisitor {
            child_depth: self.child_depth,
        })
    }
}

struct PairsVisitor {
    child_depth: usize,
}

impl<'de> Visitor<'de> for PairsVisitor {
    type Value = Vec<(Value, Value)>;

    fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
        formatter.write_str("a sequence of key/value pairs")
    }

    fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Vec<(Value, Value)>, A::Error> {
        let mut entries = Vec::new();
        while let Some(pair) = seq.next_element_seed(PairSeed {
            child_depth: self.child_depth,
        })? {
            entries.push(pair);
        }
        Ok(entries)
    }
}

#[derive(Clone, Copy)]
struct PairSeed {
    child_depth: usize,
}

impl<'de> DeserializeSeed<'de> for PairSeed {
    type Value = (Value, Value);

    fn deserialize<D: Deserializer<'de>>(
        self,
        deserializer: D,
    ) -> Result<(Value, Value), D::Error> {
        deserializer.deserialize_tuple(
            2,
            PairVisitor {
                child_depth: self.child_depth,
            },
        )
    }
}

struct PairVisitor {
    child_depth: usize,
}

impl<'de> Visitor<'de> for PairVisitor {
    type Value = (Value, Value);

    fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
        formatter.write_str("a key/value pair")
    }

    fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<(Value, Value), A::Error> {
        let depth = self.child_depth;
        let key = seq
            .next_element_seed(ValueSeed { depth })?
            .ok_or_else(|| de::Error::invalid_length(0, &"a key"))?;
        let value = seq
            .next_element_seed(ValueSeed { depth })?
            .ok_or_else(|| de::Error::invalid_length(1, &"a value"))?;
        Ok((key, value))
    }
}
