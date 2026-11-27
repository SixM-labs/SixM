//! Globals and the `Citizen` table that scripts see.

use std::path::PathBuf;
use std::rc::{Rc, Weak};
use std::time::Instant;

use mlua::{Function, Lua, LuaString, MultiValue, Table, Value as LuaValue};
use sixm_core::{
    ConvarKind, EventSource, convar_filter_matches, game_timer_ms, joaat, normalize_resource_path,
};

use crate::client;
use crate::entities;
use crate::events::handle_table;
use crate::exports;
use crate::http;
use crate::json;
use crate::kvp;
use crate::players;
use crate::resources;
use crate::runtime::{LuaRuntime, Side, bind_fn, lua_err};
use crate::sandbox::Helpers;
use crate::scheduler::delay_from_value;
use crate::state;
use crate::vector;
use crate::web;

/// Installs every global function, the `Citizen` table, `exports` and `json`.
///
/// The common globals are installed on both sides. The player and entity
/// creation functions are server-only, and the connection and game pool
/// functions are client-only.
pub(crate) fn install(runtime: &Rc<LuaRuntime>, helpers: &Helpers) -> mlua::Result<()> {
    let _ = game_timer_ms();
    let lua = &runtime.lua;
    let weak = &runtime.me;
    let globals = lua.globals();
    globals.raw_set(
        "print",
        print_function(lua, runtime.resource.clone(), helpers.tostring.clone())?,
    )?;
    globals.raw_set("source", "")?;
    install_scheduling(lua, weak, &globals, helpers)?;
    let citizen: Table = globals.raw_get("Citizen")?;
    crate::promise::install(runtime, &globals, &citizen)?;
    install_events(lua, weak, &globals)?;
    install_host(lua, weak, &globals, runtime.side)?;
    entities::install(lua, weak, &globals)?;
    kvp::install(lua, weak, &globals)?;
    match runtime.side {
        Side::Server => {
            players::install(lua, weak, &globals)?;
            entities::install_server(lua, weak, &globals)?;
            resources::install_server(lua, weak, &globals)?;
            http::install(lua, weak, &globals)?;
            web::install(lua, weak, &globals)?;
            install_server_convars(lua, weak, &globals)?;
        }
        Side::Client { local_player } => {
            client::install(runtime, &globals, local_player)?;
            entities::install_client(lua, weak, &globals)?;
        }
    }
    state::install(lua, weak, &globals)?;
    resources::install(lua, weak, &globals)?;
    vector::install(lua, &globals, helpers.type_fn.clone())?;
    globals.raw_set("exports", exports::table(runtime)?)?;
    Ok(())
}

/// Installs the thread and timer functions, and the `Citizen` table.
fn install_scheduling(
    lua: &Lua,
    weak: &Weak<LuaRuntime>,
    globals: &Table,
    helpers: &Helpers,
) -> mlua::Result<()> {
    let create_thread = bind_fn(lua, weak, |rt, func: Function| {
        let (slot, seq) = rt.spawn(func, rt.current_source.get(), rt.current_invoker())?;
        rt.schedule_next_frame(slot, seq);
        Ok(())
    })?;
    let create_thread_now = bind_fn(lua, weak, |rt, func: Function| {
        let (slot, _) = rt.spawn(func, rt.current_source.get(), rt.current_invoker())?;
        rt.run(slot, Instant::now(), MultiValue::new());
        Ok(())
    })?;
    let set_timeout = bind_fn(lua, weak, |rt, (delay, func): (LuaValue, Function)| {
        let id = rt.set_timeout(delay_from_value(&delay), func)?;
        i64::try_from(id).map_err(mlua::Error::runtime)
    })?;
    let clear_timeout = bind_fn(lua, weak, |rt, id: i64| {
        if let Ok(id) = u64::try_from(id) {
            rt.cancel_timeout(id);
        }
        Ok(())
    })?;

    let citizen = lua.create_table()?;
    let scheduling = [
        ("Wait", helpers.wait.clone()),
        ("CreateThread", create_thread.clone()),
        ("CreateThreadNow", create_thread_now.clone()),
        ("SetTimeout", set_timeout.clone()),
        ("ClearTimeout", clear_timeout.clone()),
    ];
    for (name, function) in &scheduling {
        citizen.raw_set(*name, function.clone())?;
        globals.raw_set(*name, function.clone())?;
    }
    globals.raw_set("Citizen", citizen)?;
    Ok(())
}

/// Installs the event functions.
fn install_events(lua: &Lua, weak: &Weak<LuaRuntime>, globals: &Table) -> mlua::Result<()> {
    let add_handler = bind_fn(lua, weak, |rt, (name, func): (String, Function)| {
        let key = rt.add_handler(&name, func);
        handle_table(rt, &name, key)
    })?;
    let remove_handler = bind_fn(lua, weak, |rt, handle: Table| {
        let key: i64 = handle.raw_get("key")?;
        let name: String = handle.raw_get("name")?;
        rt.remove_handler(&name, key);
        Ok(())
    })?;
    let register_net_event = bind_fn(lua, weak, |rt, (name, func): (String, Option<Function>)| {
        rt.mark_net_event(&name);
        if let Some(func) = func {
            rt.add_handler(&name, func);
        }
        Ok(())
    })?;
    let trigger_event = bind_fn(lua, weak, |rt, (name, args): (String, MultiValue)| {
        let values = rt.multi_to_values(args).map_err(lua_err)?;
        let host = rt.host()?;
        let saved: LuaValue = rt.lua.globals().raw_get("source")?;
        let outcome = host.trigger_event(&name, EventSource::Local, &values, Some(&rt.resource));
        rt.lua.globals().raw_set("source", saved)?;
        let canceled = outcome.map_err(lua_err)?;
        rt.set_last_emit_canceled(canceled);
        Ok(())
    })?;
    let cancel_event = bind_fn(lua, weak, |rt, (): ()| {
        rt.set_canceled();
        Ok(())
    })?;
    let was_event_canceled = bind_fn(lua, weak, |rt, (): ()| Ok(rt.was_canceled()))?;

    let events = [
        ("AddEventHandler", add_handler),
        ("RemoveEventHandler", remove_handler),
        ("RegisterNetEvent", register_net_event.clone()),
        ("RegisterServerEvent", register_net_event),
        ("TriggerEvent", trigger_event),
        ("CancelEvent", cancel_event),
        ("WasEventCanceled", was_event_canceled),
    ];
    for (name, function) in events {
        globals.raw_set(name, function)?;
    }
    Ok(())
}

/// Installs the resource, file, convar, command and timer functions, plus `json`.
fn install_host(
    lua: &Lua,
    weak: &Weak<LuaRuntime>,
    globals: &Table,
    side: Side,
) -> mlua::Result<()> {
    let is_duplicity_version = lua.create_function(move |_, (): ()| Ok(side == Side::Server))?;
    let current_name = bind_fn(lua, weak, |rt, (): ()| {
        rt.lua.create_string(rt.resource.as_bytes())
    })?;
    let resource_path = bind_fn(lua, weak, |rt, (name,): (String,)| {
        Ok(resource_root(rt, &name)?.map(|path| path.to_string_lossy().into_owned()))
    })?;
    let load_resource_file = bind_fn(lua, weak, |rt, (resource, path): (String, String)| {
        let Some(path) = normalize_resource_path(&path) else {
            return Ok(None);
        };
        match rt.host()?.load_resource_file(&resource, &path) {
            Some(bytes) => Ok(Some(rt.lua.create_string(&bytes)?)),
            None => Ok(None),
        }
    })?;
    let save_resource_file = bind_fn(
        lua,
        weak,
        |rt, (resource, path, data): (String, String, LuaString)| {
            if resource != rt.resource.as_ref() {
                return Ok(false);
            }
            let Some(path) = normalize_resource_path(&path) else {
                return Ok(false);
            };
            Ok(rt
                .host()?
                .save_resource_file(&resource, &path, data.as_bytes().as_ref()))
        },
    )?;
    let get_convar = bind_fn(
        lua,
        weak,
        |rt, (name, default): (String, LuaValue)| match rt
            .host()?
            .get_resource_convar(&rt.resource, &name)
        {
            Some(value) => Ok(LuaValue::String(rt.lua.create_string(&value)?)),
            None => Ok(default),
        },
    )?;
    let get_convar_int = bind_fn(lua, weak, |rt, (name, default): (String, i64)| {
        let parsed = rt
            .host()?
            .get_resource_convar(&rt.resource, &name)
            .and_then(|value| value.trim().parse::<i64>().ok());
        Ok(parsed.unwrap_or(default))
    })?;
    let get_convar_bool = bind_fn(lua, weak, |rt, (name, default): (String, bool)| {
        let parsed = rt
            .host()?
            .get_resource_convar(&rt.resource, &name)
            .and_then(|value| parse_bool(&value));
        Ok(parsed.unwrap_or(default))
    })?;
    let execute_command = bind_fn(lua, weak, |rt, (line,): (String,)| {
        rt.host()?.execute_command(&line, Some(&rt.resource));
        Ok(())
    })?;
    let register_command = bind_fn(
        lua,
        weak,
        |rt, (name, func, restricted): (String, Function, Option<bool>)| {
            let reference = rt.pin_function(func).map_err(lua_err)?;
            rt.host()?
                .register_command(&name, reference, restricted.unwrap_or(false));
            Ok(())
        },
    )?;
    let get_game_timer = bind_fn(lua, weak, |_, (): ()| Ok(game_timer_ms()))?;
    let get_hash_key = lua.create_function(|_, text: LuaString| Ok(hash_key(&text.as_bytes())))?;

    let host_functions = [
        ("GetCurrentResourceName", current_name),
        ("GetResourcePath", resource_path),
        ("LoadResourceFile", load_resource_file),
        ("SaveResourceFile", save_resource_file),
        ("GetConvar", get_convar),
        ("GetConvarInt", get_convar_int),
        ("GetConvarBool", get_convar_bool),
        ("ExecuteCommand", execute_command),
        ("RegisterCommand", register_command),
        ("GetGameTimer", get_game_timer),
        ("GetHashKey", get_hash_key),
        ("IsDuplicityVersion", is_duplicity_version),
    ];
    for (name, function) in host_functions {
        globals.raw_set(name, function)?;
    }
    install_convar_listeners(lua, weak, globals)?;
    globals.raw_set("json", json::table(lua)?)?;
    Ok(())
}

fn install_convar_listeners(
    lua: &Lua,
    weak: &Weak<LuaRuntime>,
    globals: &Table,
) -> mlua::Result<()> {
    let add_convar_listener = bind_fn(
        lua,
        weak,
        |rt, (filter, func): (Option<String>, Function)| {
            Ok(rt
                .convar_listeners
                .borrow_mut()
                .add(filter.unwrap_or_default(), func))
        },
    )?;
    let remove_convar_listener = bind_fn(lua, weak, |rt, (cookie,): (i64,)| {
        let removed = rt.convar_listeners.borrow_mut().remove(cookie);
        drop(removed);
        Ok(())
    })?;
    let registered_commands = bind_fn(lua, weak, |rt, (): ()| {
        let names = rt.host()?.registered_commands();
        let list = rt.lua.create_table_with_capacity(names.len(), 0)?;
        for (idx, name) in names.into_iter().enumerate() {
            let entry = rt.lua.create_table_with_capacity(0, 1)?;
            entry.raw_set("name", name)?;
            list.raw_seti(idx + 1, entry)?;
        }
        Ok(list)
    })?;
    globals.raw_set("AddConvarChangeListener", add_convar_listener)?;
    globals.raw_set("RemoveConvarChangeListener", remove_convar_listener)?;
    globals.raw_set("GetRegisteredCommands", registered_commands)?;
    Ok(())
}

/// Installs the convar setters, which only a server has. `SetConvar` stores a
/// plain convar, `SetConvarReplicated` a replicated one and `SetConvarServerInfo`
/// one marked as server information.
fn install_server_convars(lua: &Lua, weak: &Weak<LuaRuntime>, globals: &Table) -> mlua::Result<()> {
    let setters = [
        ("SetConvar", ConvarKind::Plain),
        ("SetConvarReplicated", ConvarKind::Replicated),
        ("SetConvarServerInfo", ConvarKind::ServerInfo),
    ];
    for (name, kind) in setters {
        let setter = bind_fn(lua, weak, move |rt, (convar, value): (String, String)| {
            players::server(&*rt.host()?)?
                .set_convar(&convar, &value, kind)
                .map_err(lua_err)
        })?;
        globals.raw_set(name, setter)?;
    }
    Ok(())
}

/// Reads a convar's text as a boolean. `true`, `1`, `yes` and `on` are true, and
/// `false`, `0`, `no` and `off` are false, in any letter case. Other text, after
/// trimming, is not a boolean.
fn parse_bool(text: &str) -> Option<bool> {
    match text.trim().to_ascii_lowercase().as_str() {
        "true" | "1" | "yes" | "on" => Some(true),
        "false" | "0" | "no" | "off" => Some(false),
        _ => None,
    }
}

/// `print`: converts each argument with the original `tostring` and logs the line.
fn print_function(
    lua: &Lua,
    resource: sixm_core::ResourceName,
    tostring: Function,
) -> mlua::Result<Function> {
    lua.create_function(move |_, args: MultiValue| {
        let mut parts = Vec::new();
        for value in args {
            let text: LuaString = tostring.call(value)?;
            parts.push(String::from_utf8_lossy(&text.as_bytes()).into_owned());
        }
        tracing::info!(target: "sixm::script", "[{resource}] {}", parts.join("\t"));
        Ok(())
    })
}

/// The `GetHashKey` value of `text`: its [`joaat`] hash as a signed 32-bit integer.
#[expect(
    clippy::cast_possible_wrap,
    reason = "the hash is reinterpreted as signed, as scripts expect from GetHashKey"
)]
fn hash_key(text: &[u8]) -> i64 {
    i64::from(joaat(text) as i32)
}

/// The root folder of `name`. The runtime's own root is used without asking the host.
fn resource_root(rt: &LuaRuntime, name: &str) -> mlua::Result<Option<PathBuf>> {
    if name == rt.resource.as_ref() {
        return Ok(Some(rt.root.clone()));
    }
    Ok(rt.host()?.resource_path(name))
}

struct ConvarListener {
    cookie: i64,
    filter: String,
    func: Function,
}

#[derive(Default)]
pub(crate) struct ConvarListeners {
    listeners: Vec<ConvarListener>,
    last_cookie: i64,
}

impl ConvarListeners {
    fn add(&mut self, filter: String, func: Function) -> i64 {
        self.last_cookie += 1;
        let cookie = self.last_cookie;
        self.listeners.push(ConvarListener {
            cookie,
            filter,
            func,
        });
        cookie
    }

    fn remove(&mut self, cookie: i64) -> Option<ConvarListener> {
        let idx = self.listeners.iter().position(|l| l.cookie == cookie)?;
        Some(self.listeners.remove(idx))
    }

    fn matching(&self, name: &str) -> Vec<Function> {
        self.listeners
            .iter()
            .filter(|l| convar_filter_matches(&l.filter, name))
            .map(|l| l.func.clone())
            .collect()
    }
}

impl LuaRuntime {
    pub(crate) fn dispatch_convar_change(
        &self,
        name: &str,
        _value: Option<&str>,
        _replicated: bool,
    ) {
        let functions = self.convar_listeners.borrow().matching(name);
        if functions.is_empty() {
            return;
        }
        let outcome: mlua::Result<()> = (|| {
            let args = MultiValue::from_vec(vec![
                LuaValue::String(self.lua.create_string(name)?),
                LuaValue::String(self.empty_string.clone()),
            ]);
            let globals = self.lua.globals();
            let saved: LuaValue = globals.raw_get("source")?;
            let result = functions.into_iter().try_for_each(|func| {
                let (slot, _) = self.spawn(func, EventSource::Local, None)?;
                self.run(slot, Instant::now(), args.clone());
                Ok(())
            });
            globals.raw_set("source", saved)?;
            result
        })();
        if let Err(err) = outcome {
            tracing::error!(
                target: "sixm::script",
                "[{}] convar change of `{name}`: {err}",
                self.resource
            );
        }
    }
}
