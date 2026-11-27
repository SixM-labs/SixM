//! The `json` global: `json.encode` and `json.decode`.
//!
//! Conversion goes straight between Lua values and `serde_json` values, so
//! encoding a function is rejected before any reference is registered.

use mlua::{Lua, LuaString, Table, Value as LuaValue};
use serde_json::Value as JsonValue;

use crate::convert::{MAX_DEPTH, sequence_len};
use crate::vector::Vector;

/// Builds the `json` table.
pub(crate) fn table(lua: &Lua) -> mlua::Result<Table> {
    let table = lua.create_table()?;
    table.raw_set(
        "encode",
        lua.create_function(|_, value: LuaValue| encode(&value))?,
    )?;
    table.raw_set(
        "decode",
        lua.create_function(|lua, text: LuaString| decode(lua, &text))?,
    )?;
    Ok(table)
}

fn encode(value: &LuaValue) -> mlua::Result<String> {
    let json = encode_value(value, 0).map_err(mlua::Error::runtime)?;
    serde_json::to_string(&json).map_err(mlua::Error::runtime)
}

fn encode_value(value: &LuaValue, depth: usize) -> Result<JsonValue, String> {
    Ok(match value {
        LuaValue::Nil => JsonValue::Null,
        LuaValue::Boolean(flag) => JsonValue::Bool(*flag),
        LuaValue::Integer(int) => JsonValue::from(*int),
        LuaValue::Number(float) => serde_json::Number::from_f64(*float)
            .map(JsonValue::Number)
            .ok_or_else(|| "cannot encode NaN or infinity".to_owned())?,
        LuaValue::String(text) => JsonValue::String(utf8(text)?),
        LuaValue::Table(table) => encode_table(table, depth + 1)?,
        LuaValue::UserData(data) if data.is::<Vector>() => {
            let vector = data.borrow::<Vector>().map_err(|err| err.to_string())?;
            encode_vector(&vector)?
        }
        other => return Err(format!("cannot encode a {} value", other.type_name())),
    })
}

/// A vector becomes an object with one field per component: `x`, `y`, `z` and `w`.
fn encode_vector(vector: &Vector) -> Result<JsonValue, String> {
    let mut object = serde_json::Map::new();
    for (name, component) in vector.fields() {
        let number = serde_json::Number::from_f64(f64::from(component))
            .ok_or_else(|| "cannot encode NaN or infinity".to_owned())?;
        object.insert(name.to_owned(), JsonValue::Number(number));
    }
    Ok(JsonValue::Object(object))
}

fn encode_table(table: &Table, depth: usize) -> Result<JsonValue, String> {
    if depth > MAX_DEPTH {
        return Err(format!("value nested deeper than {MAX_DEPTH} levels"));
    }
    let entries = table
        .pairs::<LuaValue, LuaValue>()
        .collect::<mlua::Result<Vec<_>>>()
        .map_err(|err| err.to_string())?;

    if let Some(len) = sequence_len(&entries) {
        let mut items = vec![JsonValue::Null; len];
        for (key, value) in &entries {
            if let LuaValue::Integer(index) = key {
                let encoded = encode_value(value, depth)?;
                if let Some(slot) = usize::try_from(*index - 1)
                    .ok()
                    .and_then(|index| items.get_mut(index))
                {
                    *slot = encoded;
                }
            }
        }
        return Ok(JsonValue::Array(items));
    }

    let mut object = serde_json::Map::new();
    for (key, value) in &entries {
        let name = match key {
            LuaValue::String(text) => utf8(text)?,
            LuaValue::Integer(int) => int.to_string(),
            LuaValue::Number(float) if float.is_finite() => float.to_string(),
            LuaValue::Number(_) => return Err("cannot encode NaN or infinity".to_owned()),
            _ => return Err("table keys must be strings or numbers".to_owned()),
        };
        object.insert(name, encode_value(value, depth)?);
    }
    Ok(JsonValue::Object(object))
}

fn utf8(text: &LuaString) -> Result<String, String> {
    std::str::from_utf8(&text.as_bytes())
        .map(str::to_owned)
        .map_err(|_| "cannot encode a string that is not valid UTF-8".to_owned())
}

fn decode(lua: &Lua, text: &LuaString) -> mlua::Result<LuaValue> {
    let bytes = text.as_bytes();
    let source = std::str::from_utf8(&bytes)
        .map_err(|_| mlua::Error::runtime("json.decode: input is not valid UTF-8"))?;
    let parsed: JsonValue = serde_json::from_str(source)
        .map_err(|err| mlua::Error::runtime(format!("json.decode: {err}")))?;
    from_json(lua, &parsed)
}

fn from_json(lua: &Lua, value: &JsonValue) -> mlua::Result<LuaValue> {
    Ok(match value {
        JsonValue::Null => LuaValue::Nil,
        JsonValue::Bool(flag) => LuaValue::Boolean(*flag),
        JsonValue::Number(number) => match number.as_i64() {
            Some(int) => LuaValue::Integer(int),
            None => number.as_f64().map_or(LuaValue::Nil, LuaValue::Number),
        },
        JsonValue::String(text) => LuaValue::String(lua.create_string(text)?),
        JsonValue::Array(items) => {
            let table = lua.create_table()?;
            for (index, item) in items.iter().enumerate() {
                table.raw_seti(index + 1, from_json(lua, item)?)?;
            }
            LuaValue::Table(table)
        }
        JsonValue::Object(map) => {
            let table = lua.create_table()?;
            for (key, item) in map {
                table.raw_set(key.as_str(), from_json(lua, item)?)?;
            }
            LuaValue::Table(table)
        }
    })
}
