//! Resource natives: the invoking resource, the lifecycle state of a resource,
//! the resource list and resource metadata. Both sides install them.

use std::rc::Weak;

use mlua::{Lua, Table};

use crate::runtime::{LuaRuntime, bind_fn};

/// Installs `GetInvokingResource`, `GetResourceState`, `GetNumResources`,
/// `GetResourceByFindIndex`, `GetResourceMetadata` and `GetNumResourceMetadata`.
pub(crate) fn install(lua: &Lua, weak: &Weak<LuaRuntime>, globals: &Table) -> mlua::Result<()> {
    let invoking = bind_fn(lua, weak, |rt, (): ()| match rt.current_invoker() {
        Some(name) => Ok(Some(rt.lua.create_string(name.as_bytes())?)),
        None => Ok(None),
    })?;
    let state = bind_fn(lua, weak, |rt, (name,): (String,)| {
        let state = rt.host()?.resource_state(&name);
        rt.lua.create_string(state.name())
    })?;
    let num_resources = bind_fn(lua, weak, |rt, (): ()| {
        let count = rt.host()?.resource_names().len();
        i64::try_from(count).map_err(mlua::Error::runtime)
    })?;
    let find_index = bind_fn(lua, weak, |rt, (index,): (i64,)| {
        let names = rt.host()?.resource_names();
        match nth(names, index) {
            Some(name) => Ok(Some(rt.lua.create_string(name.as_bytes())?)),
            None => Ok(None),
        }
    })?;
    let metadata = bind_fn(
        lua,
        weak,
        |rt, (resource, key, index): (String, String, i64)| {
            let values = rt.host()?.resource_metadata(&resource, &key);
            Ok(nth(values, index))
        },
    )?;
    let num_metadata = bind_fn(lua, weak, |rt, (resource, key): (String, String)| {
        let count = rt.host()?.resource_metadata(&resource, &key).len();
        i64::try_from(count).map_err(mlua::Error::runtime)
    })?;

    let functions = [
        ("GetInvokingResource", invoking),
        ("GetResourceState", state),
        ("GetNumResources", num_resources),
        ("GetResourceByFindIndex", find_index),
        ("GetResourceMetadata", metadata),
        ("GetNumResourceMetadata", num_metadata),
    ];
    for (name, function) in functions {
        globals.raw_set(name, function)?;
    }
    Ok(())
}

/// The item at the 0-based `index`, or `None` for a negative or out-of-range index.
fn nth<T>(items: Vec<T>, index: i64) -> Option<T> {
    usize::try_from(index)
        .ok()
        .and_then(|index| items.into_iter().nth(index))
}
