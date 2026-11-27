//! State bags as scripts see them: `GlobalState`, `Player(id).state`, the change
//! handlers and the bag-name helper.
//!
//! A bag handle keeps no values. Every read asks the host, so a handle can
//! never show a stale value.

use std::rc::Weak;
use std::time::Instant;

use mlua::{
    AnyUserData, FromLua, Function, Lua, MetaMethod, MultiValue, Table, UserData, UserDataMethods,
    Value as LuaValue,
};
use sixm_core::{EventSource, GLOBAL_BAG, Value, player_of_bag};

use crate::runtime::{LuaRuntime, bind_fn, lua_err};

/// The `reserved` argument of a change handler. FiveM passes it for
/// compatibility; SixM always passes `0`.
const RESERVED: i64 = 0;

/// One handler registered with `AddStateBagChangeHandler`.
struct ChangeHandler {
    cookie: i64,
    key: Option<String>,
    bag: Option<String>,
    func: Function,
}

/// Change handlers of one runtime. Lives in a `RefCell` and is never borrowed across a Lua call.
#[derive(Default)]
pub(crate) struct ChangeHandlers {
    handlers: Vec<ChangeHandler>,
    last_cookie: i64,
}

impl ChangeHandlers {
    /// Registers a handler and returns its cookie.
    fn add(&mut self, key: Option<String>, bag: Option<String>, func: Function) -> i64 {
        self.last_cookie += 1;
        let cookie = self.last_cookie;
        self.handlers.push(ChangeHandler {
            cookie,
            key,
            bag,
            func,
        });
        cookie
    }

    /// Removes the handler with `cookie`. It is returned so the caller drops it
    /// outside the borrow.
    fn remove(&mut self, cookie: i64) -> Option<ChangeHandler> {
        let index = self
            .handlers
            .iter()
            .position(|handler| handler.cookie == cookie)?;
        Some(self.handlers.remove(index))
    }

    /// The functions whose filters accept a change to `key` in `bag`, in registration order.
    fn matching(&self, bag: &str, key: &str) -> Vec<Function> {
        self.handlers
            .iter()
            .filter(|handler| {
                handler.key.as_deref().is_none_or(|wanted| wanted == key)
                    && handler.bag.as_deref().is_none_or(|wanted| wanted == bag)
            })
            .map(|handler| handler.func.clone())
            .collect()
    }
}

/// A state bag as scripts see it: its name and the runtime it belongs to.
struct StateBag {
    bag: String,
    runtime: Weak<LuaRuntime>,
}

impl UserData for StateBag {
    fn add_methods<M: UserDataMethods<Self>>(methods: &mut M) {
        methods.add_meta_method(MetaMethod::Index, |_, this, key: String| {
            let runtime = LuaRuntime::upgrade(&this.runtime)?;
            match runtime.host()?.state_get(&this.bag, &key) {
                Some(value) => runtime.to_lua(&value),
                None => Ok(LuaValue::Nil),
            }
        });
        methods.add_meta_method(
            MetaMethod::NewIndex,
            |_, this, (key, value): (String, LuaValue)| {
                let runtime = LuaRuntime::upgrade(&this.runtime)?;
                write(&runtime, &this.bag, &key, &value, true)
            },
        );
        // Methods are looked up before `__index`, so `set` is reserved: `bag.set`
        // is this method, and a state key cannot be read under that name.
        methods.add_method(
            "set",
            |_, this, (key, value, replicated): (String, LuaValue, Option<bool>)| {
                let runtime = LuaRuntime::upgrade(&this.runtime)?;
                write(
                    &runtime,
                    &this.bag,
                    &key,
                    &value,
                    replicated.unwrap_or(true),
                )
            },
        );
    }
}

/// Converts `value` and stores it under `key` in `bag` through the host.
fn write(
    runtime: &LuaRuntime,
    bag: &str,
    key: &str,
    value: &LuaValue,
    replicated: bool,
) -> mlua::Result<()> {
    let value = runtime.to_network_value(value).map_err(lua_err)?;
    runtime
        .host()?
        .state_set(bag, key, value, replicated)
        .map_err(lua_err)
}

/// The FiveM form of `SetStateBagValue`: `value` is msgpack bytes and only its
/// first `length` bytes are decoded. The decoded value is stored like any write.
fn write_packed(
    runtime: &LuaRuntime,
    bag: &str,
    key: &str,
    value: &LuaValue,
    length: i64,
    replicated: bool,
) -> mlua::Result<()> {
    let LuaValue::String(text) = value else {
        return Err(mlua::Error::runtime(
            "SetStateBagValue: valueData must be a msgpack string",
        ));
    };
    let data = text.as_bytes();
    let len = usize::try_from(length)
        .ok()
        .filter(|len| *len <= data.len())
        .ok_or_else(|| mlua::Error::runtime("SetStateBagValue: valueLength exceeds data"))?;
    let decoded = sixm_core::msgpack::unpack(&data[..len]).map_err(lua_err)?;
    runtime
        .host()?
        .state_set(bag, key, decoded, replicated)
        .map_err(lua_err)
}

/// A handle to `bag` for `runtime`.
pub(crate) fn state_bag(runtime: &LuaRuntime, bag: &str) -> mlua::Result<AnyUserData> {
    runtime.lua.create_userdata(StateBag {
        bag: bag.to_owned(),
        runtime: runtime.me.clone(),
    })
}

/// Installs `GlobalState`, `AddStateBagChangeHandler`,
/// `RemoveStateBagChangeHandler`, `GetPlayerFromStateBagName` and the FiveM
/// name-based natives `GetStateBagValue`, `StateBagHasKey`, `GetStateBagKeys`,
/// `SetStateBagValue` and `EnsureEntityStateBag`. These exist on both sides.
/// `Player(id)` is server-only and lives in `players`.
///
/// `EnsureEntityStateBag` does nothing: an entity's bag exists for the whole
/// life of the entity, so it is accepted for compatibility only.
pub(crate) fn install(lua: &Lua, weak: &Weak<LuaRuntime>, globals: &Table) -> mlua::Result<()> {
    let global = lua.create_userdata(StateBag {
        bag: GLOBAL_BAG.to_owned(),
        runtime: weak.clone(),
    })?;
    let add_change_handler = bind_fn(
        lua,
        weak,
        |rt, (key, bag, func): (Option<String>, Option<String>, Function)| {
            Ok(rt.state.borrow_mut().add(key, bag, func))
        },
    )?;
    let remove_change_handler = bind_fn(lua, weak, |rt, (cookie,): (i64,)| {
        let removed = rt.state.borrow_mut().remove(cookie);
        drop(removed);
        Ok(())
    })?;
    let player_from_bag = bind_fn(lua, weak, |_, (bag,): (String,)| {
        Ok(i64::from(player_of_bag(&bag).unwrap_or(0)))
    })?;
    let get_value = bind_fn(lua, weak, |rt, (bag, key): (String, String)| {
        match rt.host()?.state_get(&bag, &key) {
            Some(value) => rt.to_lua(&value),
            None => Ok(LuaValue::Nil),
        }
    })?;
    let has_key = bind_fn(lua, weak, |rt, (bag, key): (String, String)| {
        Ok(rt.host()?.state_get(&bag, &key).is_some())
    })?;
    let get_keys = bind_fn(lua, weak, |rt, (bag,): (String,)| {
        rt.host()?
            .state_keys(&bag)
            .map(|keys| rt.lua.create_sequence_from(keys))
            .transpose()
    })?;
    let set_value = bind_fn(
        lua,
        weak,
        |rt, (bag, key, value, fourth, fifth): (String, String, LuaValue, LuaValue, LuaValue)| {
            if let LuaValue::Integer(length) = fourth
                && !fifth.is_nil()
            {
                let replicated = bool::from_lua(fifth, &rt.lua)?;
                return write_packed(rt, &bag, &key, &value, length, replicated);
            }
            let replicated = bool::from_lua(fourth, &rt.lua)?;
            write(rt, &bag, &key, &value, replicated)
        },
    )?;
    let ensure_entity = bind_fn(lua, weak, |_, (_entity,): (i64,)| Ok(()))?;

    globals.raw_set("GlobalState", global)?;
    globals.raw_set("AddStateBagChangeHandler", add_change_handler)?;
    globals.raw_set("RemoveStateBagChangeHandler", remove_change_handler)?;
    globals.raw_set("GetPlayerFromStateBagName", player_from_bag)?;
    globals.raw_set("GetStateBagValue", get_value)?;
    globals.raw_set("StateBagHasKey", has_key)?;
    globals.raw_set("GetStateBagKeys", get_keys)?;
    globals.raw_set("SetStateBagValue", set_value)?;
    globals.raw_set("EnsureEntityStateBag", ensure_entity)?;
    Ok(())
}

impl LuaRuntime {
    /// Runs the change handlers that match `bag` and `key`. Each one starts as
    /// its own task and runs until it first yields.
    pub(crate) fn dispatch_state_change(
        &self,
        bag: &str,
        key: &str,
        value: &Value,
        replicated: bool,
    ) {
        let functions = self.state.borrow().matching(bag, key);
        if functions.is_empty() {
            return;
        }
        if let Err(err) = self.run_state_handlers(functions, bag, key, value, replicated) {
            tracing::error!(
                target: "sixm::script",
                "[{}] state change of `{bag}.{key}`: {err}",
                self.resource
            );
        }
    }

    /// Starts each handler as a task with the change's arguments. A write
    /// often comes from a handler that is already running, such as
    /// `playerJoining`, so the `source` global is saved and restored: a
    /// handler here must not change the source that the caller sees.
    fn run_state_handlers(
        &self,
        functions: Vec<Function>,
        bag: &str,
        key: &str,
        value: &Value,
        replicated: bool,
    ) -> mlua::Result<()> {
        let args = self.state_change_args(bag, key, value, replicated)?;
        let globals = self.lua.globals();
        let saved: LuaValue = globals.raw_get("source")?;
        let outcome = functions.into_iter().try_for_each(|function| {
            let (slot, _) = self.spawn(function, EventSource::Local, None)?;
            self.run(slot, Instant::now(), args.clone());
            Ok(())
        });
        globals.raw_set("source", saved)?;
        outcome
    }

    /// The `(bagName, key, value, reserved, replicated)` arguments of a change handler.
    fn state_change_args(
        &self,
        bag: &str,
        key: &str,
        value: &Value,
        replicated: bool,
    ) -> mlua::Result<MultiValue> {
        Ok(MultiValue::from_vec(vec![
            LuaValue::String(self.lua.create_string(bag)?),
            LuaValue::String(self.lua.create_string(key)?),
            self.to_lua(value)?,
            LuaValue::Integer(RESERVED),
            LuaValue::Boolean(replicated),
        ]))
    }
}
