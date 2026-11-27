//! Clean-room `MessagePack` serializer and decoder for [`Value`].
//!
//! Enforces strict size and nesting bounds so untrusted payloads cannot exhaust
//! memory or overflow the call stack.

use crate::runtime::ScriptError;
use crate::value::Value;

/// Largest payload accepted by [`pack`] or [`unpack`], in bytes (4 MiB).
pub const MAX_MSGPACK_BYTES: usize = 4 * 1024 * 1024;
/// Deepest container nesting permitted while packing or unpacking.
const MAX_DEPTH: usize = 32;

/// Extension type tags for SixM vector and quaternion values.
const EXT_VECTOR2: i8 = 20;
const EXT_VECTOR3: i8 = 21;
const EXT_VECTOR4: i8 = 22;
const EXT_QUAT: i8 = 23;

/// Serializes `value` into `MessagePack` bytes.
///
/// Function references are rejected, and output exceeding
/// [`MAX_MSGPACK_BYTES`] or [`MAX_DEPTH`] fails cleanly.
pub fn pack(value: &Value) -> Result<Vec<u8>, ScriptError> {
    let mut out = Vec::new();
    encode_value(value, 0, &mut out)?;
    if out.len() > MAX_MSGPACK_BYTES {
        return Err(ScriptError::Runtime(format!(
            "msgpack payload exceeds {MAX_MSGPACK_BYTES} bytes"
        )));
    }
    Ok(out)
}

/// Decodes `MessagePack` bytes back into a [`Value`].
///
/// Rejects malformed, trailing, oversized, or deeply nested payloads.
pub fn unpack(bytes: &[u8]) -> Result<Value, ScriptError> {
    if bytes.len() > MAX_MSGPACK_BYTES {
        return Err(ScriptError::Runtime(format!(
            "msgpack payload exceeds {MAX_MSGPACK_BYTES} bytes"
        )));
    }
    let mut cursor = Cursor::new(bytes);
    let value = decode_value(&mut cursor, 0)?;
    if !cursor.is_empty() {
        return Err(ScriptError::Runtime(
            "unexpected trailing bytes in msgpack payload".to_owned(),
        ));
    }
    Ok(value)
}

fn encode_value(value: &Value, depth: usize, out: &mut Vec<u8>) -> Result<(), ScriptError> {
    if depth > MAX_DEPTH {
        return Err(ScriptError::Runtime(format!(
            "msgpack nesting exceeds {MAX_DEPTH} levels"
        )));
    }
    if out.len() > MAX_MSGPACK_BYTES {
        return Err(ScriptError::Runtime(format!(
            "msgpack payload exceeds {MAX_MSGPACK_BYTES} bytes"
        )));
    }
    match value {
        Value::Nil => out.push(0xc0),
        Value::Bool(false) => out.push(0xc2),
        Value::Bool(true) => out.push(0xc3),
        Value::Int(n) => encode_int(*n, out),
        Value::Float(f) => {
            out.push(0xcb);
            out.extend_from_slice(&f.to_be_bytes());
        }
        Value::String(bytes) => encode_bytes(bytes, out)?,
        Value::Array(items) => {
            encode_array_len(items.len(), out)?;
            for item in items {
                encode_value(item, depth + 1, out)?;
            }
        }
        Value::Map(pairs) => {
            encode_map_len(pairs.len(), out)?;
            for (k, v) in pairs {
                encode_value(k, depth + 1, out)?;
                encode_value(v, depth + 1, out)?;
            }
        }
        Value::Vector2(v) => encode_f32s(EXT_VECTOR2, v, out),
        Value::Vector3(v) => encode_f32s(EXT_VECTOR3, v, out),
        Value::Vector4(v) => encode_f32s(EXT_VECTOR4, v, out),
        Value::Quat(v) => encode_f32s(EXT_QUAT, v, out),
        Value::FuncRef(_) => {
            return Err(ScriptError::Runtime(
                "cannot msgpack a function value".to_owned(),
            ));
        }
    }
    Ok(())
}

fn encode_int(n: i64, out: &mut Vec<u8>) {
    if (-32..=127).contains(&n) {
        #[expect(clippy::cast_sign_loss, clippy::cast_possible_truncation)]
        out.push(n as u8);
    } else if let Ok(v) = i8::try_from(n) {
        out.push(0xd0);
        out.extend_from_slice(&v.to_be_bytes());
    } else if let Ok(v) = i16::try_from(n) {
        out.push(0xd1);
        out.extend_from_slice(&v.to_be_bytes());
    } else if let Ok(v) = i32::try_from(n) {
        out.push(0xd2);
        out.extend_from_slice(&v.to_be_bytes());
    } else {
        out.push(0xd3);
        out.extend_from_slice(&n.to_be_bytes());
    }
}

fn encode_bytes(bytes: &[u8], out: &mut Vec<u8>) -> Result<(), ScriptError> {
    let len = u32::try_from(bytes.len())
        .map_err(|_| ScriptError::Runtime("msgpack string too large".to_owned()))?;
    if std::str::from_utf8(bytes).is_ok() {
        if let Ok(small) = u8::try_from(len)
            && small < 32
        {
            out.push(0xa0 | small);
        } else if let Ok(small) = u8::try_from(len) {
            out.push(0xd9);
            out.push(small);
        } else if let Ok(med) = u16::try_from(len) {
            out.push(0xda);
            out.extend_from_slice(&med.to_be_bytes());
        } else {
            out.push(0xdb);
            out.extend_from_slice(&len.to_be_bytes());
        }
    } else if let Ok(small) = u8::try_from(len) {
        out.push(0xc4);
        out.push(small);
    } else if let Ok(med) = u16::try_from(len) {
        out.push(0xc5);
        out.extend_from_slice(&med.to_be_bytes());
    } else {
        out.push(0xc6);
        out.extend_from_slice(&len.to_be_bytes());
    }
    out.extend_from_slice(bytes);
    Ok(())
}

fn encode_array_len(len: usize, out: &mut Vec<u8>) -> Result<(), ScriptError> {
    let len = u32::try_from(len)
        .map_err(|_| ScriptError::Runtime("msgpack array too large".to_owned()))?;
    if let Ok(small) = u8::try_from(len)
        && small < 16
    {
        out.push(0x90 | small);
    } else if let Ok(med) = u16::try_from(len) {
        out.push(0xdc);
        out.extend_from_slice(&med.to_be_bytes());
    } else {
        out.push(0xdd);
        out.extend_from_slice(&len.to_be_bytes());
    }
    Ok(())
}

fn encode_map_len(len: usize, out: &mut Vec<u8>) -> Result<(), ScriptError> {
    let len =
        u32::try_from(len).map_err(|_| ScriptError::Runtime("msgpack map too large".to_owned()))?;
    if let Ok(small) = u8::try_from(len)
        && small < 16
    {
        out.push(0x80 | small);
    } else if let Ok(med) = u16::try_from(len) {
        out.push(0xde);
        out.extend_from_slice(&med.to_be_bytes());
    } else {
        out.push(0xdf);
        out.extend_from_slice(&len.to_be_bytes());
    }
    Ok(())
}

fn encode_f32s(tag: i8, floats: &[f32], out: &mut Vec<u8>) {
    let byte_len = u8::try_from(floats.len() * 4).unwrap_or(0);
    if byte_len == 8 {
        out.push(0xd7);
    } else if byte_len == 16 {
        out.push(0xd8);
    } else {
        out.push(0xc7);
        out.push(byte_len);
    }
    out.push(tag.to_be_bytes()[0]);
    for value in floats {
        out.extend_from_slice(&value.to_be_bytes());
    }
}

struct Cursor<'a> {
    bytes: &'a [u8],
}

impl<'a> Cursor<'a> {
    fn new(bytes: &'a [u8]) -> Self {
        Self { bytes }
    }

    fn is_empty(&self) -> bool {
        self.bytes.is_empty()
    }

    fn remaining(&self) -> usize {
        self.bytes.len()
    }

    fn read_u8(&mut self) -> Result<u8, ScriptError> {
        let (&first, rest) = self
            .bytes
            .split_first()
            .ok_or_else(|| ScriptError::Runtime("truncated msgpack payload".to_owned()))?;
        self.bytes = rest;
        Ok(first)
    }

    fn read_exact<const N: usize>(&mut self) -> Result<[u8; N], ScriptError> {
        if self.bytes.len() < N {
            return Err(ScriptError::Runtime("truncated msgpack payload".to_owned()));
        }
        let (head, rest) = self.bytes.split_at(N);
        self.bytes = rest;
        let mut array = [0_u8; N];
        array.copy_from_slice(head);
        Ok(array)
    }

    fn read_slice(&mut self, len: usize) -> Result<&'a [u8], ScriptError> {
        if self.bytes.len() < len {
            return Err(ScriptError::Runtime("truncated msgpack payload".to_owned()));
        }
        let (head, rest) = self.bytes.split_at(len);
        self.bytes = rest;
        Ok(head)
    }
}

fn decode_value(cursor: &mut Cursor<'_>, depth: usize) -> Result<Value, ScriptError> {
    if depth > MAX_DEPTH {
        return Err(ScriptError::Runtime(format!(
            "msgpack nesting exceeds {MAX_DEPTH} levels"
        )));
    }
    let marker = cursor.read_u8()?;
    match marker {
        0x00..=0x7f => Ok(Value::Int(i64::from(marker))),
        0xe0..=0xff => Ok(Value::Int(i64::from(i8::from_be_bytes([marker])))),
        0x80..=0x8f => decode_map(cursor, usize::from(marker & 0x0f), depth),
        0x90..=0x9f => decode_array(cursor, usize::from(marker & 0x0f), depth),
        0xa0..=0xbf => decode_bytes_value(cursor, usize::from(marker & 0x1f)),
        0xc0 => Ok(Value::Nil),
        0xc2 => Ok(Value::Bool(false)),
        0xc3 => Ok(Value::Bool(true)),
        0xc4 | 0xd9 => {
            let len = usize::from(cursor.read_u8()?);
            decode_bytes_value(cursor, len)
        }
        0xc5 | 0xda => {
            let len = usize::from(u16::from_be_bytes(cursor.read_exact()?));
            decode_bytes_value(cursor, len)
        }
        0xc6 | 0xdb => {
            let len = read_u32_len(cursor)?;
            decode_bytes_value(cursor, len)
        }
        0xc7 => {
            let len = usize::from(cursor.read_u8()?);
            decode_ext(cursor, len)
        }
        0xc8 => {
            let len = usize::from(u16::from_be_bytes(cursor.read_exact()?));
            decode_ext(cursor, len)
        }
        0xc9 => {
            let len = read_u32_len(cursor)?;
            decode_ext(cursor, len)
        }
        0xca..=0xd3 => decode_number(cursor, marker),
        0xd4 => decode_ext(cursor, 1),
        0xd5 => decode_ext(cursor, 2),
        0xd6 => decode_ext(cursor, 4),
        0xd7 => decode_ext(cursor, 8),
        0xd8 => decode_ext(cursor, 16),
        0xdc => {
            let len = usize::from(u16::from_be_bytes(cursor.read_exact()?));
            decode_array(cursor, len, depth)
        }
        0xdd => {
            let len = read_u32_len(cursor)?;
            decode_array(cursor, len, depth)
        }
        0xde => {
            let len = usize::from(u16::from_be_bytes(cursor.read_exact()?));
            decode_map(cursor, len, depth)
        }
        0xdf => {
            let len = read_u32_len(cursor)?;
            decode_map(cursor, len, depth)
        }
        0xc1 => Err(ScriptError::Runtime(
            "invalid msgpack format byte 0xc1".to_owned(),
        )),
    }
}

fn read_u32_len(cursor: &mut Cursor<'_>) -> Result<usize, ScriptError> {
    usize::try_from(u32::from_be_bytes(cursor.read_exact()?))
        .map_err(|_| ScriptError::Runtime("msgpack length overflow".to_owned()))
}

fn decode_number(cursor: &mut Cursor<'_>, marker: u8) -> Result<Value, ScriptError> {
    match marker {
        0xca => Ok(Value::Float(f64::from(f32::from_be_bytes(
            cursor.read_exact()?,
        )))),
        0xcb => Ok(Value::Float(f64::from_be_bytes(cursor.read_exact()?))),
        0xcc => Ok(Value::Int(i64::from(cursor.read_u8()?))),
        0xcd => Ok(Value::Int(i64::from(u16::from_be_bytes(
            cursor.read_exact()?,
        )))),
        0xce => Ok(Value::Int(i64::from(u32::from_be_bytes(
            cursor.read_exact()?,
        )))),
        0xcf => {
            let raw = u64::from_be_bytes(cursor.read_exact()?);
            i64::try_from(raw)
                .map(Value::Int)
                .map_err(|_| ScriptError::Runtime("msgpack u64 exceeds i64 range".to_owned()))
        }
        0xd0 => Ok(Value::Int(i64::from(i8::from_be_bytes(
            cursor.read_exact()?,
        )))),
        0xd1 => Ok(Value::Int(i64::from(i16::from_be_bytes(
            cursor.read_exact()?,
        )))),
        0xd2 => Ok(Value::Int(i64::from(i32::from_be_bytes(
            cursor.read_exact()?,
        )))),
        0xd3 => Ok(Value::Int(i64::from_be_bytes(cursor.read_exact()?))),
        _ => unreachable!(),
    }
}

fn decode_bytes_value(cursor: &mut Cursor<'_>, len: usize) -> Result<Value, ScriptError> {
    let slice = cursor.read_slice(len)?;
    Ok(Value::String(slice.into()))
}

fn decode_array(cursor: &mut Cursor<'_>, len: usize, depth: usize) -> Result<Value, ScriptError> {
    if len > cursor.remaining() {
        return Err(ScriptError::Runtime(
            "truncated msgpack array payload".to_owned(),
        ));
    }
    let mut items = Vec::with_capacity(len.min(1024));
    for _ in 0..len {
        items.push(decode_value(cursor, depth + 1)?);
    }
    Ok(Value::Array(items))
}

fn decode_map(cursor: &mut Cursor<'_>, len: usize, depth: usize) -> Result<Value, ScriptError> {
    if len.saturating_mul(2) > cursor.remaining() {
        return Err(ScriptError::Runtime(
            "truncated msgpack map payload".to_owned(),
        ));
    }
    let mut pairs = Vec::with_capacity(len.min(1024));
    for _ in 0..len {
        let key = decode_value(cursor, depth + 1)?;
        let val = decode_value(cursor, depth + 1)?;
        pairs.push((key, val));
    }
    Ok(Value::Map(pairs))
}

fn decode_ext(cursor: &mut Cursor<'_>, len: usize) -> Result<Value, ScriptError> {
    let tag = i8::from_be_bytes([cursor.read_u8()?]);
    let data = cursor.read_slice(len)?;
    match tag {
        EXT_VECTOR2 if data.len() == 8 => {
            Ok(Value::Vector2([read_f32(data, 0), read_f32(data, 4)]))
        }
        EXT_VECTOR3 if data.len() == 12 => Ok(Value::Vector3([
            read_f32(data, 0),
            read_f32(data, 4),
            read_f32(data, 8),
        ])),
        EXT_VECTOR4 if data.len() == 16 => Ok(Value::Vector4([
            read_f32(data, 0),
            read_f32(data, 4),
            read_f32(data, 8),
            read_f32(data, 12),
        ])),
        EXT_QUAT if data.len() == 16 => Ok(Value::Quat([
            read_f32(data, 0),
            read_f32(data, 4),
            read_f32(data, 8),
            read_f32(data, 12),
        ])),
        _ => Ok(Value::String(data.into())),
    }
}

fn read_f32(data: &[u8], offset: usize) -> f32 {
    let mut bytes = [0_u8; 4];
    bytes.copy_from_slice(&data[offset..offset + 4]);
    f32::from_be_bytes(bytes)
}

#[cfg(test)]
mod tests {
    use super::{MAX_DEPTH, pack, unpack};
    use crate::{FuncRef, Value};
    use std::sync::Arc;

    #[test]
    fn round_trips_all_supported_value_types() {
        let sample = Value::Map(vec![
            (Value::from("nil"), Value::Nil),
            (Value::from("ok"), Value::Bool(true)),
            (Value::from("small_neg"), Value::Int(-5)),
            (Value::from("big_int"), Value::Int(-1_000_000_000_000)),
            (Value::from("pi"), Value::Float(3.25)),
            (
                Value::from("bin"),
                Value::String(Box::from([0xff_u8, 0x00, 0xfe])),
            ),
            (
                Value::from("list"),
                Value::Array(vec![Value::Int(1), Value::from("two")]),
            ),
            (Value::from("v2"), Value::Vector2([1.5, -2.5])),
            (Value::from("v3"), Value::Vector3([1.0, 2.0, 3.0])),
            (Value::from("v4"), Value::Vector4([1.0, 2.0, 3.0, 4.0])),
            (Value::from("q"), Value::Quat([1.0, 0.0, 0.0, 0.0])),
        ]);
        let packed = pack(&sample).expect("pack");
        assert_eq!(unpack(&packed).expect("unpack"), sample);
    }

    #[test]
    fn rejects_functions_trailing_bytes_excessive_depth_and_oversized_counts() {
        let func = Value::FuncRef(FuncRef {
            owner: Arc::from("res"),
            instance: 1,
            id: 2,
        });
        assert!(pack(&func).is_err());

        let mut bytes = pack(&Value::Int(7)).expect("pack");
        bytes.push(0x00);
        assert!(unpack(&bytes).is_err());

        // Malicious array32 declaring 4 billion elements on a 5-byte payload.
        assert!(unpack(&[0xdd, 0xff, 0xff, 0xff, 0xff]).is_err());

        let mut deep = Value::Int(1);
        for _ in 0..=MAX_DEPTH + 1 {
            deep = Value::Array(vec![deep]);
        }
        assert!(pack(&deep).is_err());
    }
}
