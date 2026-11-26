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
use sixm_core::Value;

use crate::error::NetError;
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
const CLIENT_VARIANTS: &[&str] = &["Hello", "Event"];
const SERVER_VARIANTS: &[&str] = &["Welcome", "Rejected", "Event", "Kicked"];

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
            other => Err(de::Error::unknown_variant(
                &other.to_string(),
                SERVER_VARIANTS,
            )),
        }
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
}
