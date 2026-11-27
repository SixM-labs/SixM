//! `SetHttpHandler` and the `req` / `res` JS objects a handler receives.

use std::cell::RefCell;
use std::rc::{Rc, Weak};

use rquickjs::function::Opt;
use rquickjs::{Ctx, Exception, Function, Object, Value as JsValue};
use sixm_core::{Host, ScriptError, WebReply, WebRequest};

use crate::convert::bytes_to_js;
use crate::http::read_headers;
use crate::natives::data_bytes;
use crate::runtime::{CALLBACK_BUDGET, JsRuntime, js_err};
use crate::sandbox::catch_script;

/// Installs `SetHttpHandler((req, res) => { ... })`, server-only.
pub(crate) fn install<'js>(ctx: &Ctx<'js>, weak: &Weak<JsRuntime>) -> rquickjs::Result<()> {
    let weak = weak.clone();
    let set = Function::new(
        ctx.clone(),
        move |ctx: Ctx<'js>, func: Function<'js>| -> rquickjs::Result<()> {
            let rt = JsRuntime::upgrade(&ctx, &weak)?;
            let reference = rt
                .pin_function(&ctx, func)
                .map_err(|err| Exception::throw_message(&ctx, &err.to_string()))?;
            let host = rt.host(&ctx)?;
            let srv = host.server().ok_or_else(|| {
                Exception::throw_message(&ctx, "SetHttpHandler is only available on the server")
            })?;
            srv.set_web_handler(&rt.resource, reference);
            Ok(())
        },
    )?;
    ctx.globals().set("SetHttpHandler", set)
}

impl JsRuntime {
    pub(crate) fn invoke_web(
        &self,
        id: u32,
        request: u64,
        data: &WebRequest,
    ) -> Result<(), ScriptError> {
        let handler = self
            .lookup_function(id)
            .ok_or_else(|| ScriptError::StaleRef(self.func_ref(id)))?;
        let _budget = self.enter_budget(CALLBACK_BUDGET);
        let outcome = self.with_ctx(|ctx| {
            let body_handler: Rc<RefCell<Option<Function<'_>>>> = Rc::default();
            let result = self.run_web(ctx, &handler, request, data, &body_handler);
            let leftover = body_handler.borrow_mut().take();
            drop(leftover);
            result
        });
        self.drain_jobs();
        outcome
    }

    fn run_web<'js>(
        &self,
        ctx: &Ctx<'js>,
        handler: &rquickjs::Persistent<Function<'static>>,
        request: u64,
        data: &WebRequest,
        body_handler: &Rc<RefCell<Option<Function<'js>>>>,
    ) -> Result<(), ScriptError> {
        let handler = handler.clone().restore(ctx).map_err(|err| js_err(ctx, err))?;
        let req = build_request(ctx, data, body_handler).map_err(|err| js_err(ctx, err))?;
        let res = build_response(ctx, self.host.clone(), request).map_err(|err| js_err(ctx, err))?;
        catch_script(ctx, handler.call::<_, ()>((req, res)))?;
        let body = body_handler.borrow_mut().take();
        if let Some(body_fn) = body {
            let arg = bytes_to_js(ctx, &data.body)?;
            catch_script(ctx, body_fn.call::<_, ()>((arg,)))?;
        }
        Ok(())
    }
}

fn build_request<'js>(
    ctx: &Ctx<'js>,
    data: &WebRequest,
    body_handler: &Rc<RefCell<Option<Function<'js>>>>,
) -> rquickjs::Result<Object<'js>> {
    let req = Object::new(ctx.clone())?;
    req.set("method", data.method.as_str())?;
    req.set("path", data.path.as_str())?;
    req.set("query", data.query.as_str())?;
    let headers = Object::new(ctx.clone())?;
    for (name, value) in &data.headers {
        headers.set(name.as_str(), value.as_str())?;
    }
    req.set("headers", headers)?;

    let slot = Rc::clone(body_handler);
    let set_data = Function::new(
        ctx.clone(),
        move |ctx: Ctx<'js>, cb: JsValue<'js>| -> rquickjs::Result<()> {
            let Some(func) = cb.into_function() else {
                return Err(Exception::throw_type(
                    &ctx,
                    "setDataHandler needs a function to call with the body",
                ));
            };
            *slot.borrow_mut() = Some(func);
            Ok(())
        },
    )?;
    req.set("setDataHandler", set_data)?;
    Ok(req)
}

fn build_response<'js>(
    ctx: &Ctx<'js>,
    host: Weak<dyn Host>,
    request: u64,
) -> rquickjs::Result<Object<'js>> {
    let res = Object::new(ctx.clone())?;

    let head_host = host.clone();
    res.set(
        "writeHead",
        Function::new(
            ctx.clone(),
            move |ctx: Ctx<'js>,
                  status: JsValue<'js>,
                  headers: Opt<Option<JsValue<'js>>>|
                  -> rquickjs::Result<()> {
                let status = status_of(&ctx, &status)?;
                let headers = match headers.0.flatten() {
                    None => Vec::new(),
                    Some(val) => {
                        let Some(obj) = val.as_object() else {
                            return Err(Exception::throw_type(
                                &ctx,
                                &format!("the headers must be an object, not {}", val.type_name()),
                            ));
                        };
                        read_headers(&ctx, obj)?
                    }
                };
                reply(&ctx, &head_host, request, WebReply::Head { status, headers })
            },
        )?,
    )?;

    let write_host = host.clone();
    res.set(
        "write",
        Function::new(
            ctx.clone(),
            move |ctx: Ctx<'js>, data: Opt<Option<JsValue<'js>>>| -> rquickjs::Result<()> {
                let bytes = body_of(&ctx, data.0.flatten().as_ref())?;
                reply(&ctx, &write_host, request, WebReply::Write(bytes))
            },
        )?,
    )?;

    res.set(
        "send",
        Function::new(
            ctx.clone(),
            move |ctx: Ctx<'js>, data: Opt<Option<JsValue<'js>>>| -> rquickjs::Result<()> {
                let bytes = body_of(&ctx, data.0.flatten().as_ref())?;
                reply(&ctx, &host, request, WebReply::End(bytes))
            },
        )?,
    )?;

    Ok(res)
}

fn status_of(ctx: &Ctx<'_>, value: &JsValue<'_>) -> rquickjs::Result<u16> {
    let code = crate::players::whole_number(value)
        .ok_or_else(|| Exception::throw_type(ctx, "the status code must be an integer"))?;
    u16::try_from(code)
        .ok()
        .filter(|code| (100..=999).contains(code))
        .ok_or_else(|| Exception::throw_range(ctx, &format!("{code} is not an HTTP status code")))
}

fn body_of(ctx: &Ctx<'_>, value: Option<&JsValue<'_>>) -> rquickjs::Result<Vec<u8>> {
    match value {
        None => Ok(Vec::new()),
        Some(val) if val.is_null() || val.is_undefined() => Ok(Vec::new()),
        Some(val) => data_bytes(ctx, val),
    }
}

fn reply(
    ctx: &Ctx<'_>,
    host: &Weak<dyn Host>,
    request: u64,
    step: WebReply,
) -> rquickjs::Result<()> {
    let host = host
        .upgrade()
        .ok_or_else(|| Exception::throw_message(ctx, "the server is no longer available"))?;
    let srv = host
        .server()
        .ok_or_else(|| Exception::throw_message(ctx, "HTTP responses need a server"))?;
    srv.web_reply(request, step)
        .map_err(|err| Exception::throw_message(ctx, &err.to_string()))
}
