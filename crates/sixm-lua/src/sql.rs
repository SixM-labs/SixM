//! Server-only database bridge used by the oxmysql compatibility resource.

use std::rc::Weak;

use mlua::{Function, Lua, Table, Value as LuaValue};
use sixm_core::{FuncRef, ResourceName, ScriptError, ServerHost, SqlQueryKind, Value};

use crate::runtime::{LuaRuntime, bind_fn, lua_err};

pub(crate) fn install(lua: &Lua, weak: &Weak<LuaRuntime>, globals: &Table) -> mlua::Result<()> {
    globals.raw_set(
        "SixmSqlIsReady",
        bind_fn(lua, weak, |rt, (): ()| {
            Ok(rt.host()?.server().is_some_and(ServerHost::sql_is_ready))
        })?,
    )?;
    globals.raw_set(
        "SixmSqlExecute",
        bind_fn(
            lua,
            weak,
            |rt, (kind, sql, params, callback): (String, String, LuaValue, Function)| {
                let kind: SqlQueryKind = kind.parse().map_err(lua_err)?;
                submit(rt, &params, callback, |server, origin, params, callback| {
                    server.sql_execute(origin, kind, sql, params, callback)
                })
            },
        )?,
    )?;
    globals.raw_set(
        "SixmSqlPrepare",
        bind_fn(
            lua,
            weak,
            |rt, (sql, params, callback): (String, LuaValue, Function)| {
                submit(rt, &params, callback, |server, origin, params, callback| {
                    server.sql_prepare(origin, sql, params, callback)
                })
            },
        )?,
    )?;
    globals.raw_set(
        "SixmSqlTransaction",
        bind_fn(
            lua,
            weak,
            |rt, (queries, params, callback): (LuaValue, LuaValue, Function)| {
                let queries = rt.to_network_value(&queries).map_err(lua_err)?;
                submit(rt, &params, callback, |server, origin, params, callback| {
                    server.sql_transaction(origin, queries, params, callback)
                })
            },
        )?,
    )
}

fn submit(
    rt: &LuaRuntime,
    params: &LuaValue,
    callback: Function,
    run: impl FnOnce(&dyn ServerHost, &ResourceName, Value, FuncRef) -> Result<(), ScriptError>,
) -> mlua::Result<()> {
    let host = rt.host()?;
    let server = host
        .server()
        .ok_or_else(|| mlua::Error::runtime("SQL is only available on the server"))?;
    let params = rt.to_network_value(params).map_err(lua_err)?;
    let origin = rt.current_invoker();
    let reference = rt.pin_function(callback).map_err(lua_err)?;
    let result = run(
        server,
        origin.as_ref().unwrap_or(&rt.resource),
        params,
        reference.clone(),
    );
    if result.is_err() {
        rt.release_function(reference.id);
    }
    result.map_err(lua_err)
}
