//! The resource key-value functions: `SetResourceKvp` and its typed variants,
//! `GetResourceKvp*`, `DeleteResourceKvp`, the `StartFindKvp` cursor and
//! `FlushResourceKvp`. Each resource reads and writes only its own store.

use std::collections::HashMap;
use std::rc::Weak;

use mlua::{Lua, LuaString, Table};

use crate::runtime::{LuaRuntime, bind_fn};

/// An open `StartFindKvp` cursor: the keys it found and the next one to return.
#[derive(Debug)]
pub(crate) struct Find {
    keys: Vec<String>,
    next: usize,
}

/// The open cursors of one runtime, by handle.
#[derive(Debug, Default)]
pub(crate) struct Finds {
    last_handle: i64,
    open: HashMap<i64, Find>,
}

/// Installs the KVP functions. Every write is batched, so the `NoSync` variants
/// are the same functions as their plain names.
pub(crate) fn install(lua: &Lua, weak: &Weak<LuaRuntime>, globals: &Table) -> mlua::Result<()> {
    let get_string = bind_fn(lua, weak, |rt, (key,): (String,)| {
        match rt.host()?.kvp_get(rt.resource.as_ref(), &key) {
            Some(bytes) => Ok(Some(rt.lua.create_string(&bytes)?)),
            None => Ok(None),
        }
    })?;
    let get_int = bind_fn(lua, weak, |rt, (key,): (String,)| {
        Ok(stored_text(rt, &key)?
            .and_then(|text| text.trim().parse::<i64>().ok())
            .unwrap_or(0))
    })?;
    let get_float = bind_fn(lua, weak, |rt, (key,): (String,)| {
        Ok(stored_text(rt, &key)?
            .and_then(|text| text.trim().parse::<f64>().ok())
            .unwrap_or(0.0))
    })?;

    let set_string = bind_fn(lua, weak, |rt, (key, value): (String, LuaString)| {
        Ok(rt
            .host()?
            .kvp_set(rt.resource.as_ref(), &key, value.as_bytes().as_ref()))
    })?;
    let set_int = bind_fn(lua, weak, |rt, (key, value): (String, i64)| {
        Ok(rt
            .host()?
            .kvp_set(rt.resource.as_ref(), &key, value.to_string().as_bytes()))
    })?;
    let set_float = bind_fn(lua, weak, |rt, (key, value): (String, f64)| {
        Ok(rt
            .host()?
            .kvp_set(rt.resource.as_ref(), &key, value.to_string().as_bytes()))
    })?;
    let delete = bind_fn(lua, weak, |rt, (key,): (String,)| {
        Ok(rt.host()?.kvp_delete(rt.resource.as_ref(), &key))
    })?;

    let start_find = bind_fn(lua, weak, |rt, (prefix,): (String,)| {
        let keys = rt.host()?.kvp_keys(rt.resource.as_ref(), &prefix);
        let mut finds = rt.kvp_finds.borrow_mut();
        finds.last_handle += 1;
        let handle = finds.last_handle;
        finds.open.insert(handle, Find { keys, next: 0 });
        Ok(handle)
    })?;
    let find = bind_fn(lua, weak, |rt, (handle,): (i64,)| {
        let mut finds = rt.kvp_finds.borrow_mut();
        let Some(cursor) = finds.open.get_mut(&handle) else {
            return Ok(None);
        };
        let key = cursor.keys.get(cursor.next).cloned();
        if key.is_some() {
            cursor.next += 1;
        }
        Ok(key)
    })?;
    let end_find = bind_fn(lua, weak, |rt, (handle,): (i64,)| {
        rt.kvp_finds.borrow_mut().open.remove(&handle);
        Ok(())
    })?;
    let flush = bind_fn(lua, weak, |rt, (): ()| {
        rt.host()?.kvp_flush();
        Ok(())
    })?;

    let functions = [
        ("GetResourceKvpString", get_string),
        ("GetResourceKvpInt", get_int),
        ("GetResourceKvpFloat", get_float),
        ("SetResourceKvp", set_string.clone()),
        ("SetResourceKvpNoSync", set_string),
        ("SetResourceKvpInt", set_int.clone()),
        ("SetResourceKvpIntNoSync", set_int),
        ("SetResourceKvpFloat", set_float.clone()),
        ("SetResourceKvpFloatNoSync", set_float),
        ("DeleteResourceKvp", delete.clone()),
        ("DeleteResourceKvpNoSync", delete),
        ("StartFindKvp", start_find),
        ("FindKvp", find),
        ("EndFindKvp", end_find),
        ("FlushResourceKvp", flush),
    ];
    for (name, function) in functions {
        globals.raw_set(name, function)?;
    }
    Ok(())
}

/// The stored value of `key` as text, or `None` when unset or not UTF-8.
fn stored_text(rt: &LuaRuntime, key: &str) -> mlua::Result<Option<String>> {
    let bytes = rt.host()?.kvp_get(rt.resource.as_ref(), key);
    Ok(bytes.and_then(|bytes| String::from_utf8(bytes).ok()))
}
