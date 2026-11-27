//! The `msgpack` global: `msgpack.pack` and `msgpack.unpack`.
//!
//! Conversion goes straight between Lua values and [`sixm_core::Value`], so
//! packing a function is rejected before any reference is registered.

use mlua::{Lua, LuaString, Table, Value as LuaValue};
use sixm_core::{ScriptError, Value};

use crate::convert::{nested, sequence_len};
use crate::runtime::{IntoScriptResult, lua_err};
use crate::vector::{Quat, Vector};

/// Builds the `msgpack` table.
pub(crate) fn table(lua: &Lua) -> mlua::Result<Table> {
    let table = lua.create_table()?;
    table.raw_set(
        "pack",
        lua.create_function(|lua, value: LuaValue| pack(lua, &value))?,
    )?;
    table.raw_set(
        "unpack",
        lua.create_function(|lua, bytes: LuaString| unpack(lua, &bytes))?,
    )?;
    Ok(table)
}

fn pack(lua: &Lua, value: &LuaValue) -> mlua::Result<LuaString> {
    let core_value = lua_to_value(value, 0).map_err(lua_err)?;
    let bytes = sixm_core::msgpack::pack(&core_value).map_err(lua_err)?;
    lua.create_string(&bytes)
}

fn unpack(lua: &Lua, bytes: &LuaString) -> mlua::Result<LuaValue> {
    let raw = bytes.as_bytes();
    let value = sixm_core::msgpack::unpack(&raw).map_err(lua_err)?;
    value_to_lua(lua, &value, 0)
}

fn lua_to_value(value: &LuaValue, depth: usize) -> Result<Value, ScriptError> {
    Ok(match value {
        LuaValue::Nil => Value::Nil,
        LuaValue::Boolean(flag) => Value::Bool(*flag),
        LuaValue::Integer(int) => Value::Int(*int),
        LuaValue::Number(float) => Value::Float(*float),
        LuaValue::String(text) => Value::String(Box::from(&*text.as_bytes())),
        LuaValue::Table(table) => table_to_value(table, depth)?,
        LuaValue::UserData(data) if data.is::<Vector>() => {
            data.borrow::<Vector>().into_script()?.to_value()
        }
        LuaValue::UserData(data) if data.is::<Quat>() => {
            data.borrow::<Quat>().into_script()?.to_value()
        }
        other => {
            return Err(ScriptError::Runtime(format!(
                "cannot pack a {} value",
                other.type_name()
            )));
        }
    })
}

fn table_to_value(table: &Table, depth: usize) -> Result<Value, ScriptError> {
    let depth = nested(depth)?;
    let entries = table
        .pairs::<LuaValue, LuaValue>()
        .collect::<mlua::Result<Vec<_>>>()
        .into_script()?;
    if let Some(len) = sequence_len(&entries) {
        let mut items = vec![Value::Nil; len];
        for (key, item) in &entries {
            if let LuaValue::Integer(index) = key {
                let converted = lua_to_value(item, depth)?;
                if let Some(slot) = usize::try_from(*index - 1)
                    .ok()
                    .and_then(|idx| items.get_mut(idx))
                {
                    *slot = converted;
                }
            }
        }
        return Ok(Value::Array(items));
    }
    let pairs = entries
        .iter()
        .map(|(key, item)| Ok((lua_to_value(key, depth)?, lua_to_value(item, depth)?)))
        .collect::<Result<Vec<(Value, Value)>, ScriptError>>()?;
    Ok(Value::Map(pairs))
}

fn value_to_lua(lua: &Lua, value: &Value, depth: usize) -> mlua::Result<LuaValue> {
    Ok(match value {
        Value::Nil => LuaValue::Nil,
        Value::Bool(flag) => LuaValue::Boolean(*flag),
        Value::Int(int) => LuaValue::Integer(*int),
        Value::Float(float) => LuaValue::Number(*float),
        Value::String(bytes) => LuaValue::String(lua.create_string(bytes)?),
        Value::Array(items) => {
            let depth = nested(depth).map_err(lua_err)?;
            let table = lua.create_table_with_capacity(items.len(), 0)?;
            for (index, item) in items.iter().enumerate() {
                table.raw_seti(index + 1, value_to_lua(lua, item, depth)?)?;
            }
            LuaValue::Table(table)
        }
        Value::Map(pairs) => {
            let depth = nested(depth).map_err(lua_err)?;
            let table = lua.create_table_with_capacity(0, pairs.len())?;
            for (key, item) in pairs {
                table.raw_set(
                    value_to_lua(lua, key, depth)?,
                    value_to_lua(lua, item, depth)?,
                )?;
            }
            LuaValue::Table(table)
        }
        Value::Vector2(components) => {
            LuaValue::UserData(lua.create_userdata(Vector::new(*components))?)
        }
        Value::Vector3(components) => {
            LuaValue::UserData(lua.create_userdata(Vector::new(*components))?)
        }
        Value::Vector4(components) => {
            LuaValue::UserData(lua.create_userdata(Vector::new(*components))?)
        }
        Value::Quat(components) => LuaValue::UserData(lua.create_userdata(Quat::new(*components))?),
        Value::FuncRef(_) => {
            return Err(mlua::Error::runtime("cannot unpack a function reference"));
        }
    })
}
