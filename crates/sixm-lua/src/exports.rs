//! The `exports` global: local exports, cross-resource proxies, and export calls.

use std::collections::HashMap;

use mlua::{Function, MultiValue, Table, Value as LuaValue};
use sixm_core::{ScriptError, Value};

use crate::runtime::{IntoScriptResult, LuaRuntime, bind_fn, lua_err};

/// Exports registered by this resource, and the proxy table cached per resource.
#[derive(Default)]
pub(crate) struct Exports {
    local: HashMap<String, Function>,
    proxies: HashMap<String, Table>,
}

/// Builds the `exports` table: calling it registers an export, and indexing it
/// with a resource name returns that resource's proxy.
pub(crate) fn table(runtime: &LuaRuntime) -> mlua::Result<Table> {
    let lua = &runtime.lua;
    let exports = lua.create_table()?;
    let meta = lua.create_table()?;

    meta.raw_set(
        "__call",
        bind_fn(
            lua,
            &runtime.me,
            |runtime, (_this, name, func): (LuaValue, String, Function)| {
                let previous = runtime.exports.borrow_mut().local.insert(name, func);
                drop(previous);
                Ok(())
            },
        )?,
    )?;
    meta.raw_set(
        "__index",
        bind_fn(
            lua,
            &runtime.me,
            |runtime, (_this, resource): (LuaValue, String)| runtime.proxy_for(&resource),
        )?,
    )?;
    exports.set_metatable(Some(meta))?;
    Ok(exports)
}

impl LuaRuntime {
    /// Returns the cached proxy for `resource`, creating it on first use. Each
    /// method is built once and then stored in the proxy itself.
    fn proxy_for(&self, resource: &str) -> mlua::Result<Table> {
        let cached = self.exports.borrow().proxies.get(resource).cloned();
        if let Some(proxy) = cached {
            return Ok(proxy);
        }

        let proxy = self.lua.create_table()?;
        let meta = self.lua.create_table()?;
        let remote = resource.to_owned();
        meta.raw_set(
            "__index",
            bind_fn(
                &self.lua,
                &self.me,
                move |runtime, (proxy, method): (Table, String)| {
                    let function = runtime.export_method(remote.clone(), method.clone())?;
                    proxy.raw_set(method, function.clone())?;
                    Ok(function)
                },
            )?,
        )?;
        proxy.set_metatable(Some(meta))?;
        self.exports
            .borrow_mut()
            .proxies
            .insert(resource.to_owned(), proxy.clone());
        Ok(proxy)
    }

    /// Whether `table` is the cached proxy for `resource`.
    fn is_proxy(&self, resource: &str, table: &Table) -> bool {
        self.exports
            .borrow()
            .proxies
            .get(resource)
            .is_some_and(|proxy| proxy.to_pointer() == table.to_pointer())
    }

    /// Builds the function `proxy.method`. Called with the proxy as its first
    /// argument (the colon form), that argument is dropped.
    fn export_method(&self, resource: String, method: String) -> mlua::Result<Function> {
        bind_fn(&self.lua, &self.me, move |runtime, args: MultiValue| {
            let mut args = args.into_vec();
            let via_colon = matches!(
                args.first(),
                Some(LuaValue::Table(first)) if runtime.is_proxy(&resource, first)
            );
            if via_colon {
                args.remove(0);
            }
            let values = runtime.to_values(&args).map_err(lua_err)?;
            let host = runtime.host()?;
            let results = host
                .call_export(&resource, &method, &values)
                .map_err(lua_err)?;
            runtime.to_lua_multi(&results)
        })
    }

    /// Finds the function behind an export: a registered export first, then a
    /// declared global function of the same name.
    fn find_export(&self, name: &str) -> Result<Function, ScriptError> {
        let local = self.exports.borrow().local.get(name).cloned();
        if let Some(function) = local {
            return Ok(function);
        }
        if self.declared_exports.contains(name) {
            let global: mlua::Result<Option<Function>> = self.lua.globals().raw_get(name);
            if let Ok(Some(function)) = global {
                return Ok(function);
            }
        }
        Err(ScriptError::NoSuchExport {
            resource: self.resource.to_string(),
            export: name.to_owned(),
        })
    }

    /// Calls an export synchronously, outside any task.
    pub(crate) fn invoke_export(
        &self,
        export: &str,
        args: &[Value],
    ) -> Result<Vec<Value>, ScriptError> {
        let function = self.find_export(export)?;
        let lua_args = self.to_lua_multi(args).into_script()?;
        let results: MultiValue = function.call(lua_args).into_script()?;
        self.multi_to_values(results)
    }
}
