//! Conversions between Lua values and [`sixm_core::Value`].

use mlua::{MultiValue, Table, Value as LuaValue};
use sixm_core::{FuncRef, ScriptError, Value};

use crate::refs::FuncHandle;
use crate::runtime::{IntoScriptResult, LuaRuntime, lua_err};
use crate::vector::{Quat, Vector};

/// Whether functions may be converted into references.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Functions {
    /// Register them, for calls between resources.
    Share,
    /// Fail, for values sent over the network.
    Refuse,
}

/// Deepest table nesting allowed to cross a resource boundary.
pub(crate) const MAX_DEPTH: usize = 64;

/// Returns the length of `entries` if their keys are exactly `1..=len`.
pub(crate) fn sequence_len(entries: &[(LuaValue, LuaValue)]) -> Option<usize> {
    let len = entries.len();
    let is_sequence = entries.iter().all(|(key, _)| {
        matches!(key, LuaValue::Integer(index)
            if usize::try_from(*index).is_ok_and(|index| (1..=len).contains(&index)))
    });
    is_sequence.then_some(len)
}

/// Builds the error for a Lua value that cannot cross a resource boundary.
pub(crate) fn foreign_type(value: &LuaValue) -> ScriptError {
    ScriptError::Runtime(format!(
        "cannot pass a {} value to another resource",
        value.type_name()
    ))
}

/// Returns the depth one table deeper than `depth`, or an error past [`MAX_DEPTH`].
pub(crate) fn nested(depth: usize) -> Result<usize, ScriptError> {
    let depth = depth + 1;
    if depth > MAX_DEPTH {
        Err(ScriptError::TooDeep(MAX_DEPTH))
    } else {
        Ok(depth)
    }
}

impl LuaRuntime {
    /// Converts Lua arguments or results into script values.
    pub(crate) fn to_values(&self, values: &[LuaValue]) -> Result<Vec<Value>, ScriptError> {
        values
            .iter()
            .map(|value| self.value_at(value, 0, Functions::Share))
            .collect()
    }

    /// Converts values bound for the network, where functions cannot go. A
    /// function is refused before any reference is registered for it.
    pub(crate) fn to_network_values(&self, values: &[LuaValue]) -> Result<Vec<Value>, ScriptError> {
        values
            .iter()
            .map(|value| self.to_network_value(value))
            .collect()
    }

    /// Converts one value bound for the network. See [`to_network_values`](Self::to_network_values).
    pub(crate) fn to_network_value(&self, value: &LuaValue) -> Result<Value, ScriptError> {
        self.value_at(value, 0, Functions::Refuse)
    }

    /// Converts a list of script values into a Lua multi-value.
    pub(crate) fn to_lua_multi(&self, values: &[Value]) -> mlua::Result<MultiValue> {
        let items = values
            .iter()
            .map(|value| self.to_lua(value))
            .collect::<mlua::Result<Vec<_>>>()?;
        Ok(MultiValue::from_vec(items))
    }

    /// Converts a Lua value into a script value. Functions are registered and
    /// returned as references.
    fn value_at(
        &self,
        value: &LuaValue,
        depth: usize,
        functions: Functions,
    ) -> Result<Value, ScriptError> {
        Ok(match value {
            LuaValue::Nil => Value::Nil,
            LuaValue::Boolean(flag) => Value::Bool(*flag),
            LuaValue::Integer(int) => Value::Int(*int),
            LuaValue::Number(float) => Value::Float(*float),
            LuaValue::String(text) => Value::String(Box::from(&*text.as_bytes())),
            LuaValue::Table(table) => self.table_value(table, depth, functions)?,
            // A vector is data, so it crosses the network as well as between resources.
            LuaValue::UserData(data) if data.is::<Vector>() => {
                data.borrow::<Vector>().into_script()?.to_value()
            }
            LuaValue::UserData(data) if data.is::<Quat>() => {
                data.borrow::<Quat>().into_script()?.to_value()
            }
            LuaValue::Function(_) | LuaValue::UserData(_) if functions == Functions::Refuse => {
                return Err(ScriptError::Runtime(
                    "functions cannot be sent over the network".to_owned(),
                ));
            }
            LuaValue::Function(function) => Value::FuncRef(self.ref_function(function.clone())?),
            LuaValue::UserData(data) => match data.borrow::<FuncHandle>() {
                Ok(handle) => Value::FuncRef(handle.func.clone()),
                Err(_) => return Err(foreign_type(value)),
            },
            // An error raised by a host function reaches Lua as an error object.
            // It crosses as its message, the way a string error would.
            LuaValue::Error(error) => Value::from(error.to_string()),
            other => return Err(foreign_type(other)),
        })
    }

    fn table_value(
        &self,
        table: &Table,
        depth: usize,
        functions: Functions,
    ) -> Result<Value, ScriptError> {
        let depth = nested(depth)?;
        let entries = table
            .pairs::<LuaValue, LuaValue>()
            .collect::<mlua::Result<Vec<_>>>()
            .into_script()?;
        if let Some(len) = sequence_len(&entries) {
            let mut items = vec![Value::Nil; len];
            for (key, item) in &entries {
                if let LuaValue::Integer(index) = key {
                    let converted = self.value_at(item, depth, functions)?;
                    if let Some(slot) = usize::try_from(*index - 1)
                        .ok()
                        .and_then(|index| items.get_mut(index))
                    {
                        *slot = converted;
                    }
                }
            }
            return Ok(Value::Array(items));
        }
        let pairs = entries
            .iter()
            .map(|(key, item)| {
                Ok((
                    self.value_at(key, depth, functions)?,
                    self.value_at(item, depth, functions)?,
                ))
            })
            .collect::<Result<Vec<(Value, Value)>, ScriptError>>()?;
        Ok(Value::Map(pairs))
    }

    /// Converts a script value into a Lua value.
    pub(crate) fn to_lua(&self, value: &Value) -> mlua::Result<LuaValue> {
        self.lua_at(value, 0)
    }

    fn lua_at(&self, value: &Value, depth: usize) -> mlua::Result<LuaValue> {
        Ok(match value {
            Value::Nil => LuaValue::Nil,
            Value::Bool(flag) => LuaValue::Boolean(*flag),
            Value::Int(int) => LuaValue::Integer(*int),
            Value::Float(float) => LuaValue::Number(*float),
            Value::String(bytes) => LuaValue::String(self.lua.create_string(bytes)?),
            Value::Array(items) => {
                let depth = nested(depth).map_err(lua_err)?;
                let table = self.lua.create_table()?;
                for (index, item) in items.iter().enumerate() {
                    table.raw_seti(index + 1, self.lua_at(item, depth)?)?;
                }
                LuaValue::Table(table)
            }
            Value::Map(pairs) => {
                let depth = nested(depth).map_err(lua_err)?;
                let table = self.lua.create_table()?;
                for (key, item) in pairs {
                    table.raw_set(self.lua_at(key, depth)?, self.lua_at(item, depth)?)?;
                }
                LuaValue::Table(table)
            }
            Value::Vector2(components) => {
                LuaValue::UserData(self.lua.create_userdata(Vector::new(*components))?)
            }
            Value::Vector3(components) => {
                LuaValue::UserData(self.lua.create_userdata(Vector::new(*components))?)
            }
            Value::Vector4(components) => {
                LuaValue::UserData(self.lua.create_userdata(Vector::new(*components))?)
            }
            Value::Quat(components) => {
                LuaValue::UserData(self.lua.create_userdata(Quat::new(*components))?)
            }
            Value::FuncRef(func) => self.lua_function(func)?,
        })
    }

    /// A function owned by this runtime becomes the original function. Any other
    /// owner gets a callable handle that retains the function while it lives.
    fn lua_function(&self, func: &FuncRef) -> mlua::Result<LuaValue> {
        let stale = || lua_err(ScriptError::StaleRef(func.clone()));
        if func.owner == self.resource && func.instance == self.instance {
            let function = self.lookup_function(func.id).ok_or_else(stale)?;
            return Ok(LuaValue::Function(function));
        }
        let host = self.host()?;
        if !host.retain_ref(func) {
            return Err(stale());
        }
        let handle = FuncHandle {
            host: self.host.clone(),
            func: func.clone(),
            runtime: self.me.clone(),
        };
        Ok(LuaValue::UserData(self.lua.create_userdata(handle)?))
    }

    /// Converts a Lua multi-value (such as a call's results) into script values.
    pub(crate) fn multi_to_values(&self, values: MultiValue) -> Result<Vec<Value>, ScriptError> {
        self.to_values(&values.into_vec())
    }
}
