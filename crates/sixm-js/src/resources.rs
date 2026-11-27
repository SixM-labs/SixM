//! Resource natives: `GetInvokingResource`, `GetResourceState`,
//! `GetNumResources`, `GetResourceByFindIndex`, `GetResourceMetadata`,
//! `GetNumResourceMetadata`, `GetResourceCommands`, `StartResource`, `StopResource`,
//! and `RestartResource`.

use std::rc::Weak;

use rquickjs::{Ctx, Exception, Function, IntoJs, Object, Value as JsValue};
use sixm_core::ServerHost;

use crate::players::server;
use crate::runtime::JsRuntime;

pub(crate) fn install<'js>(ctx: &Ctx<'js>, weak: &Weak<JsRuntime>) -> rquickjs::Result<()> {
    let globals = ctx.globals();

    {
        let weak = weak.clone();
        globals.set(
            "GetInvokingResource",
            Function::new(
                ctx.clone(),
                move |ctx: Ctx<'js>| -> rquickjs::Result<JsValue<'js>> {
                    let rt = JsRuntime::upgrade(&ctx, &weak)?;
                    match rt.current_invoker() {
                        Some(name) => name.as_ref().into_js(&ctx),
                        None => Ok(JsValue::new_null(ctx)),
                    }
                },
            )?,
        )?;
    }

    {
        let weak = weak.clone();
        globals.set(
            "GetResourceState",
            Function::new(
                ctx.clone(),
                move |ctx: Ctx<'js>, name: String| -> rquickjs::Result<&'static str> {
                    let rt = JsRuntime::upgrade(&ctx, &weak)?;
                    Ok(rt.host(&ctx)?.resource_state(&name).name())
                },
            )?,
        )?;
    }

    {
        let weak = weak.clone();
        globals.set(
            "GetNumResources",
            Function::new(ctx.clone(), move |ctx: Ctx<'js>| -> rquickjs::Result<i64> {
                let rt = JsRuntime::upgrade(&ctx, &weak)?;
                let count = rt.host(&ctx)?.resource_names().len();
                i64::try_from(count).map_err(|err| Exception::throw_message(&ctx, &err.to_string()))
            })?,
        )?;
    }

    {
        let weak = weak.clone();
        globals.set(
            "GetResourceByFindIndex",
            Function::new(
                ctx.clone(),
                move |ctx: Ctx<'js>, index: i64| -> rquickjs::Result<Option<String>> {
                    let rt = JsRuntime::upgrade(&ctx, &weak)?;
                    let names = rt.host(&ctx)?.resource_names();
                    Ok(nth(names, index).map(|name| name.as_ref().to_owned()))
                },
            )?,
        )?;
    }

    {
        let weak = weak.clone();
        globals.set(
            "GetResourceMetadata",
            Function::new(
                ctx.clone(),
                move |ctx: Ctx<'js>,
                      resource: String,
                      key: String,
                      index: i64|
                      -> rquickjs::Result<Option<String>> {
                    let rt = JsRuntime::upgrade(&ctx, &weak)?;
                    let values = rt.host(&ctx)?.resource_metadata(&resource, &key);
                    Ok(nth(values, index))
                },
            )?,
        )?;
    }

    {
        let weak = weak.clone();
        globals.set(
            "GetNumResourceMetadata",
            Function::new(
                ctx.clone(),
                move |ctx: Ctx<'js>, resource: String, key: String| -> rquickjs::Result<i64> {
                    let rt = JsRuntime::upgrade(&ctx, &weak)?;
                    let count = rt.host(&ctx)?.resource_metadata(&resource, &key).len();
                    i64::try_from(count)
                        .map_err(|err| Exception::throw_message(&ctx, &err.to_string()))
                },
            )?,
        )?;
    }
    install_commands(ctx, weak)
}

/// Installs `GetResourceCommands`, the console commands a resource registered.
fn install_commands<'js>(ctx: &Ctx<'js>, weak: &Weak<JsRuntime>) -> rquickjs::Result<()> {
    let globals = ctx.globals();
    let weak = weak.clone();
    globals.set(
        "GetResourceCommands",
        Function::new(
            ctx.clone(),
            move |ctx: Ctx<'js>, resource: String| -> rquickjs::Result<Vec<String>> {
                let rt = JsRuntime::upgrade(&ctx, &weak)?;
                Ok(rt.host(&ctx)?.resource_commands(&resource))
            },
        )?,
    )
}

pub(crate) fn install_server(ctx: &Ctx<'_>, weak: &Weak<JsRuntime>) -> rquickjs::Result<()> {
    let globals = ctx.globals();
    install_lifecycle_native(&globals, ctx, weak, "StartResource", |s, c, r| {
        s.start_resource(c, r)
    })?;
    install_lifecycle_native(&globals, ctx, weak, "StopResource", |s, c, r| {
        s.stop_resource(c, r)
    })?;
    install_lifecycle_native(&globals, ctx, weak, "RestartResource", |s, c, r| {
        s.restart_resource(c, r)
    })?;
    Ok(())
}

fn install_lifecycle_native<'js>(
    globals: &Object<'js>,
    ctx: &Ctx<'js>,
    weak: &Weak<JsRuntime>,
    fn_name: &str,
    action: fn(&dyn ServerHost, &str, &str) -> bool,
) -> rquickjs::Result<()> {
    let weak = weak.clone();
    globals.set(
        fn_name,
        Function::new(
            ctx.clone(),
            move |ctx: Ctx<'js>, name: String| -> rquickjs::Result<bool> {
                let rt = JsRuntime::upgrade(&ctx, &weak)?;
                Ok(action(
                    server(&ctx, &*rt.host(&ctx)?)?,
                    rt.resource.as_ref(),
                    &name,
                ))
            },
        )?,
    )
}

fn nth<T>(items: Vec<T>, index: i64) -> Option<T> {
    usize::try_from(index)
        .ok()
        .and_then(|idx| items.into_iter().nth(idx))
}
