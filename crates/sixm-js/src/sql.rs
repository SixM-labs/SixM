//! Server-only database bridge used by the oxmysql compatibility resource.

use std::rc::Weak;

use rquickjs::{Ctx, Exception, Function, Value as JsValue};
use sixm_core::{FuncRef, ResourceName, ScriptError, ServerHost, SqlQueryKind, Value};

use crate::runtime::JsRuntime;

pub(crate) fn install<'js>(ctx: &Ctx<'js>, weak: &Weak<JsRuntime>) -> rquickjs::Result<()> {
    let globals = ctx.globals();
    let runtime = weak.clone();
    globals.set(
        "SixmSqlIsReady",
        Function::new(
            ctx.clone(),
            move |ctx: Ctx<'js>| -> rquickjs::Result<bool> {
                let rt = JsRuntime::upgrade(&ctx, &runtime)?;
                Ok(rt
                    .host(&ctx)?
                    .server()
                    .is_some_and(ServerHost::sql_is_ready))
            },
        )?,
    )?;
    let runtime = weak.clone();
    globals.set(
        "SixmSqlExecute",
        Function::new(
            ctx.clone(),
            move |ctx: Ctx<'js>, kind: String, sql: String, params, callback| {
                let rt = JsRuntime::upgrade(&ctx, &runtime)?;
                let kind: SqlQueryKind = kind.parse().map_err(|err| sql_error(&ctx, &err))?;
                submit(
                    &ctx,
                    &rt,
                    &params,
                    callback,
                    |server, origin, params, callback| {
                        server.sql_execute(origin, kind, sql, params, callback)
                    },
                )
            },
        )?,
    )?;
    let runtime = weak.clone();
    globals.set(
        "SixmSqlPrepare",
        Function::new(
            ctx.clone(),
            move |ctx: Ctx<'js>, sql: String, params, callback| {
                let rt = JsRuntime::upgrade(&ctx, &runtime)?;
                submit(
                    &ctx,
                    &rt,
                    &params,
                    callback,
                    |server, origin, params, callback| {
                        server.sql_prepare(origin, sql, params, callback)
                    },
                )
            },
        )?,
    )?;
    let runtime = weak.clone();
    globals.set(
        "SixmSqlTransaction",
        Function::new(
            ctx.clone(),
            move |ctx: Ctx<'js>, queries: JsValue<'js>, params, callback| {
                let rt = JsRuntime::upgrade(&ctx, &runtime)?;
                let queries = rt
                    .to_network_value(&ctx, &queries)
                    .map_err(|err| sql_error(&ctx, &err))?;
                submit(
                    &ctx,
                    &rt,
                    &params,
                    callback,
                    |server, origin, params, callback| {
                        server.sql_transaction(origin, queries, params, callback)
                    },
                )
            },
        )?,
    )
}

fn submit<'js>(
    ctx: &Ctx<'js>,
    rt: &JsRuntime,
    params: &JsValue<'js>,
    callback: Function<'js>,
    run: impl FnOnce(&dyn ServerHost, &ResourceName, Value, FuncRef) -> Result<(), ScriptError>,
) -> rquickjs::Result<()> {
    let host = rt.host(ctx)?;
    let server = host
        .server()
        .ok_or_else(|| Exception::throw_message(ctx, "SQL is only available on the server"))?;
    let params = rt
        .to_network_value(ctx, params)
        .map_err(|err| sql_error(ctx, &err))?;
    let origin = rt.current_invoker();
    let reference = rt
        .pin_function(ctx, callback)
        .map_err(|err| sql_error(ctx, &err))?;
    let result = run(
        server,
        origin.as_ref().unwrap_or(&rt.resource),
        params,
        reference.clone(),
    );
    if result.is_err() {
        rt.release_function(reference.id);
    }
    result.map_err(|err| sql_error(ctx, &err))
}

fn sql_error(ctx: &Ctx<'_>, err: &ScriptError) -> rquickjs::Error {
    Exception::throw_message(ctx, &err.to_string())
}
