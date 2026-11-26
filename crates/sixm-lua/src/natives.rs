//! Globals and the `Citizen` table that scripts see.

use std::path::{Component, Path, PathBuf};
use std::rc::{Rc, Weak};
use std::sync::OnceLock;
use std::time::Instant;

use mlua::{Function, Lua, LuaString, MultiValue, Table, Value as LuaValue};
use sixm_core::EventSource;

use crate::events::handle_table;
use crate::exports;
use crate::json;
use crate::players;
use crate::runtime::{LuaRuntime, bind_fn, lua_err};
use crate::sandbox::Helpers;
use crate::scheduler::delay_from_value;
use crate::state;

/// Process-wide clock behind `GetGameTimer`.
static START: OnceLock<Instant> = OnceLock::new();

/// Installs every global function, the `Citizen` table, `exports` and `json`.
pub(crate) fn install(runtime: &Rc<LuaRuntime>, helpers: &Helpers) -> mlua::Result<()> {
    START.get_or_init(Instant::now);
    let lua = &runtime.lua;
    let weak = &runtime.me;
    let globals = lua.globals();
    globals.raw_set(
        "print",
        print_function(lua, runtime.resource.clone(), helpers.tostring.clone())?,
    )?;
    globals.raw_set("source", "")?;
    install_scheduling(lua, weak, &globals, helpers)?;
    install_events(lua, weak, &globals)?;
    install_host(lua, weak, &globals)?;
    players::install(lua, weak, &globals)?;
    state::install(lua, weak, &globals)?;
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
        let (slot, seq) = rt.spawn(func, rt.current_source.get())?;
        rt.schedule_next_frame(slot, seq);
        Ok(())
    })?;
    let create_thread_now = bind_fn(lua, weak, |rt, func: Function| {
        let (slot, _) = rt.spawn(func, rt.current_source.get())?;
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
        let outcome = host.trigger_event(&name, EventSource::Local, &values);
        rt.lua.globals().raw_set("source", saved)?;
        outcome.map(|_| ()).map_err(lua_err)
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
fn install_host(lua: &Lua, weak: &Weak<LuaRuntime>, globals: &Table) -> mlua::Result<()> {
    let current_name = bind_fn(lua, weak, |rt, (): ()| {
        rt.lua.create_string(rt.resource.as_bytes())
    })?;
    let resource_path = bind_fn(lua, weak, |rt, (name,): (String,)| {
        Ok(resource_root(rt, &name)?.map(|path| path.to_string_lossy().into_owned()))
    })?;
    let load_resource_file = bind_fn(lua, weak, |rt, (resource, path): (String, String)| {
        let Some(root) = resource_root(rt, &resource)? else {
            return Ok(None);
        };
        let Some(full) = safe_join(&root, &path) else {
            return Ok(None);
        };
        match std::fs::read(full) {
            Ok(bytes) => Ok(Some(rt.lua.create_string(&bytes)?)),
            Err(_) => Ok(None),
        }
    })?;
    let save_resource_file = bind_fn(
        lua,
        weak,
        |rt, (resource, path, data): (String, String, LuaString)| {
            if resource != rt.resource.as_ref() {
                return Ok(false);
            }
            let Some(full) = safe_join(&rt.root, &path) else {
                return Ok(false);
            };
            if let Some(parent) = full.parent() {
                if std::fs::create_dir_all(parent).is_err() {
                    return Ok(false);
                }
            }
            Ok(std::fs::write(full, data.as_bytes().as_ref()).is_ok())
        },
    )?;
    let get_convar = bind_fn(
        lua,
        weak,
        |rt, (name, default): (String, LuaValue)| match rt.host()?.get_convar(&name) {
            Some(value) => Ok(LuaValue::String(rt.lua.create_string(&value)?)),
            None => Ok(default),
        },
    )?;
    let get_convar_int = bind_fn(lua, weak, |rt, (name, default): (String, i64)| {
        let parsed = rt
            .host()?
            .get_convar(&name)
            .and_then(|value| value.trim().parse::<i64>().ok());
        Ok(parsed.unwrap_or(default))
    })?;
    let execute_command = bind_fn(lua, weak, |rt, (line,): (String,)| {
        rt.host()?.execute_command(&line);
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

    let host_functions = [
        ("GetCurrentResourceName", current_name),
        ("GetResourcePath", resource_path),
        ("LoadResourceFile", load_resource_file),
        ("SaveResourceFile", save_resource_file),
        ("GetConvar", get_convar),
        ("GetConvarInt", get_convar_int),
        ("ExecuteCommand", execute_command),
        ("RegisterCommand", register_command),
        ("GetGameTimer", get_game_timer),
    ];
    for (name, function) in host_functions {
        globals.raw_set(name, function)?;
    }

    globals.raw_set("json", json::table(lua)?)?;
    Ok(())
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

/// Milliseconds since the process first asked for the game timer.
fn game_timer_ms() -> i64 {
    let elapsed = START.get_or_init(Instant::now).elapsed();
    i64::try_from(elapsed.as_millis()).unwrap_or(i64::MAX)
}

/// The root folder of `name`. The runtime's own root is used without asking the host.
fn resource_root(rt: &LuaRuntime, name: &str) -> mlua::Result<Option<PathBuf>> {
    if name == rt.resource.as_ref() {
        return Ok(Some(rt.root.clone()));
    }
    Ok(rt.host()?.resource_path(name))
}

/// Joins a relative path onto `root`, rejecting absolute paths and `..`.
fn safe_join(root: &Path, relative: &str) -> Option<PathBuf> {
    let path = Path::new(relative);
    let plain = path
        .components()
        .all(|component| matches!(component, Component::Normal(_) | Component::CurDir));
    plain.then(|| root.join(path))
}
