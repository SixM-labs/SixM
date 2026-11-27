//! The `exports` global: callable ES6 `Proxy` for registering local exports
//! and invoking exports on any running resource across Lua and JS.

use std::collections::HashMap;
use std::rc::Weak;

use rquickjs::function::Rest;
use rquickjs::proxy::{Proxy, ProxyHandler, ProxyProperty, ProxyReceiver, ProxyTarget};
use rquickjs::{Ctx, Exception, Function, Object, Persistent, Value as JsValue};
use sixm_core::{ResourceName, ScriptError, Value};

use crate::convert::values_to_js_return;
use crate::runtime::{CALLBACK_BUDGET, JsRuntime, js_err};
use crate::sandbox::catch_script;

/// Exports registered by this resource, and cached per-resource `Proxy` objects.
#[derive(Default)]
pub(crate) struct Exports {
    local: HashMap<String, Persistent<Function<'static>>>,
    proxies: HashMap<String, Persistent<JsValue<'static>>>,
}

/// Installs the callable `exports` `Proxy` on `globalThis`.
pub(crate) fn install<'js>(ctx: &Ctx<'js>, weak: &Weak<JsRuntime>) -> rquickjs::Result<()> {
    let target = Function::new(ctx.clone(), || {})?;
    let handler_obj = Object::new(ctx.clone())?;

    {
        let weak = weak.clone();
        handler_obj.set(
            "apply",
            Function::new(
                ctx.clone(),
                move |ctx: Ctx<'js>,
                      _target: JsValue<'js>,
                      _this: JsValue<'js>,
                      args: Vec<JsValue<'js>>|
                      -> rquickjs::Result<()> {
                    let rt = JsRuntime::upgrade(&ctx, &weak)?;
                    let Some((name_val, func_val)) = args.first().zip(args.get(1)) else {
                        return Err(Exception::throw_type(
                            &ctx,
                            "exports(name, fn) requires a name and a function",
                        ));
                    };
                    let name = name_val
                        .as_string()
                        .and_then(|s| s.to_string().ok())
                        .ok_or_else(|| {
                            Exception::throw_type(&ctx, "export name must be a string")
                        })?;
                    let func = func_val.as_function().cloned().ok_or_else(|| {
                        Exception::throw_type(&ctx, "export value must be a function")
                    })?;
                    let previous = rt
                        .exports
                        .borrow_mut()
                        .local
                        .insert(name, Persistent::save(&ctx, func));
                    drop(previous);
                    Ok(())
                },
            )?,
        )?;
    }

    {
        let weak = weak.clone();
        handler_obj.set(
            "get",
            Function::new(
                ctx.clone(),
                move |_target: ProxyTarget<'js>,
                      prop: ProxyProperty<'js>,
                      _recv: JsValue<'js>|
                      -> rquickjs::Result<JsValue<'js>> {
                    let ctx = prop.0.ctx().clone();
                    if prop.is_symbol() {
                        return Ok(JsValue::new_undefined(ctx));
                    }
                    let Ok(resource) = prop.to_string() else {
                        return Ok(JsValue::new_undefined(ctx));
                    };
                    let rt = JsRuntime::upgrade(&ctx, &weak)?;
                    rt.proxy_for(&ctx, &resource)
                },
            )?,
        )?;
    }

    let handler = ProxyHandler::from_object(handler_obj)?;
    let proxy = Proxy::new(ctx.clone(), target, handler)?;
    ctx.globals().set("exports", proxy)
}

impl JsRuntime {
    fn proxy_for<'js>(&self, ctx: &Ctx<'js>, resource: &str) -> rquickjs::Result<JsValue<'js>> {
        if let Some(cached) = self.exports.borrow().proxies.get(resource).cloned() {
            return cached.restore(ctx);
        }

        let target = Object::new(ctx.clone())?;
        let weak = self.me.clone();
        let remote = resource.to_owned();
        let handler = ProxyHandler::new(ctx.clone())?.with_getter(
            move |target: ProxyTarget<'js>,
                  prop: ProxyProperty<'js>,
                  _recv: ProxyReceiver<'js>|
                  -> rquickjs::Result<JsValue<'js>> {
                let ctx = target.0.ctx().clone();
                if prop.is_symbol() {
                    return Ok(JsValue::new_undefined(ctx));
                }
                let Ok(method) = prop.to_string() else {
                    return Ok(JsValue::new_undefined(ctx));
                };
                if let Ok(existing) = target.0.get::<_, JsValue<'js>>(method.as_str())
                    && !existing.is_undefined()
                {
                    return Ok(existing);
                }
                let rt = JsRuntime::upgrade(&ctx, &weak)?;
                let func = rt.export_method(&ctx, remote.clone(), method.clone())?;
                target.0.set(method, func.clone())?;
                Ok(func.into_value())
            },
        )?;

        let proxy = Proxy::new(ctx.clone(), target, handler)?.into_value();
        self.exports
            .borrow_mut()
            .proxies
            .insert(resource.to_owned(), Persistent::save(ctx, proxy.clone()));
        Ok(proxy)
    }

    fn export_method<'js>(
        &self,
        ctx: &Ctx<'js>,
        resource: String,
        method: String,
    ) -> rquickjs::Result<Function<'js>> {
        let weak = self.me.clone();
        Function::new(
            ctx.clone(),
            move |ctx: Ctx<'js>, args: Rest<JsValue<'js>>| -> rquickjs::Result<JsValue<'js>> {
                let rt = JsRuntime::upgrade(&ctx, &weak)?;
                let values = rt
                    .to_values(&ctx, &args.0)
                    .map_err(|err| Exception::throw_message(&ctx, &err.to_string()))?;
                let results = if resource == rt.resource.as_ref() {
                    match rt.invoke_export_in_ctx(&ctx, &method, &values, Some(&rt.resource)) {
                        Ok(res) => res,
                        Err(ScriptError::NoSuchExport { .. }) => rt
                            .host(&ctx)?
                            .call_export(&resource, &method, &values, Some(&rt.resource))
                            .map_err(|err| Exception::throw_message(&ctx, &err.to_string()))?,
                        Err(err) => return Err(Exception::throw_message(&ctx, &err.to_string())),
                    }
                } else {
                    rt.host(&ctx)?
                        .call_export(&resource, &method, &values, Some(&rt.resource))
                        .map_err(|err| Exception::throw_message(&ctx, &err.to_string()))?
                };
                values_to_js_return(&rt, &ctx, &results)
                    .map_err(|err| Exception::throw_message(&ctx, &err.to_string()))
            },
        )
    }

    /// Finds a locally registered export or a declared global function.
    fn find_export<'js>(&self, ctx: &Ctx<'js>, name: &str) -> Result<Function<'js>, ScriptError> {
        let local = self.exports.borrow().local.get(name).cloned();
        if let Some(func) = local {
            return func.restore(ctx).map_err(|err| js_err(ctx, err));
        }
        if self.declared_exports.contains(name)
            && let Ok(val) = ctx.globals().get::<_, JsValue<'js>>(name)
            && let Some(func) = val.into_function()
        {
            return Ok(func);
        }
        Err(ScriptError::NoSuchExport {
            resource: self.resource.to_string(),
            export: name.to_owned(),
        })
    }

    /// Calls `export` synchronously, setting `GetInvokingResource()` to `invoker`.
    pub(crate) fn invoke_export(
        &self,
        export: &str,
        args: &[Value],
        invoker: Option<&ResourceName>,
    ) -> Result<Vec<Value>, ScriptError> {
        if self.in_ctx.get() {
            return Err(ScriptError::Runtime(
                "re-entrant call into active JS runtime is not supported".to_owned(),
            ));
        }
        let outcome = self.with_ctx(|ctx| self.invoke_export_in_ctx(ctx, export, args, invoker));
        self.drain_jobs();
        outcome
    }

    pub(crate) fn invoke_export_in_ctx(
        &self,
        ctx: &Ctx<'_>,
        export: &str,
        args: &[Value],
        invoker: Option<&ResourceName>,
    ) -> Result<Vec<Value>, ScriptError> {
        let _budget = self.enter_budget(CALLBACK_BUDGET);
        let outer_invoker = std::mem::replace(&mut *self.current_invoker.borrow_mut(), invoker.cloned());
        let outcome = (|| {
            let func = self.find_export(ctx, export)?;
            let js_args = self.values_to_js(ctx, args)?;
            let mut call_args = rquickjs::function::Args::new(ctx.clone(), js_args.len());
            call_args
                .push_args(js_args)
                .map_err(|err| js_err(ctx, err))?;
            let result: JsValue<'_> = catch_script(ctx, func.call_arg(call_args))?;
            self.js_return_to_values(ctx, &result)
        })();
        *self.current_invoker.borrow_mut() = outer_invoker;
        outcome
    }
}
