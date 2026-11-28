//! `SetHttpHandler`, and the request and response objects a handler is given.
//!
//! A handler is called as `handler(req, res)`. `req` has `address`, `method`,
//! `path`, `query` and `headers`; `req.setDataHandler(fn)` registers a function
//! that is given the body once the handler returns, and `req.setCancelHandler(fn)`
//! registers a function called if the client disconnects or the request times out.
//! `res` has `writeHead(status, headers)`, `write(data)` and `send(data)`, which
//! completes the response. The functions work with both `.` and `:` call syntax.

use std::cell::{Cell, RefCell};
use std::rc::{Rc, Weak};

use mlua::{Function, Lua, MultiValue, Table, Value as LuaValue};
use sixm_core::{Host, ScriptError, WebReply, WebRequest};

use crate::http::read_headers;
use crate::runtime::{IntoScriptResult, LuaRuntime, bind_fn, lua_err};

/// A registered `req.setCancelHandler` callback and the shared open flag of its request.
pub(crate) struct CancelHandler {
    pub(crate) func: Option<Function>,
    pub(crate) open: Rc<Cell<bool>>,
}

/// Installs `SetHttpHandler(function(req, res) ... end)`, which registers the
/// calling resource's HTTP handler.
pub(crate) fn install(lua: &Lua, weak: &Weak<LuaRuntime>, globals: &Table) -> mlua::Result<()> {
    let set = bind_fn(lua, weak, |rt, func: Function| {
        let reference = rt.pin_function(func).map_err(lua_err)?;
        let host = rt.host()?;
        let server = host.server().ok_or_else(|| {
            mlua::Error::runtime("SetHttpHandler is only available on the server")
        })?;
        server.set_web_handler(&rt.resource, reference);
        Ok(())
    })?;
    globals.raw_set("SetHttpHandler", set)
}

impl LuaRuntime {
    /// Runs the HTTP handler `id` for request `request`: calls it with the
    /// request and its response object, then the body handler it registered,
    /// if any, with the whole body.
    pub(crate) fn invoke_web(
        &self,
        id: u32,
        request: u64,
        data: &WebRequest,
    ) -> Result<(), ScriptError> {
        let handler = self
            .lookup_function(id)
            .ok_or_else(|| ScriptError::StaleRef(self.func_ref(id)))?;
        let body_handler: Rc<RefCell<Option<Function>>> = Rc::default();
        let open = Rc::new(Cell::new(true));
        self.http_cancels.borrow_mut().insert(
            request,
            CancelHandler {
                func: None,
                open: Rc::clone(&open),
            },
        );
        let outcome = self
            .budget
            .run(|| self.run_web(&handler, request, data, &body_handler, &open));
        // Released here, so that a body handler which captures the request
        // cannot keep it alive past this call.
        let leftover = body_handler.borrow_mut().take();
        drop(leftover);
        if outcome.is_err() {
            open.set(false);
            self.http_cancels.borrow_mut().remove(&request);
        }
        outcome.into_script()
    }

    pub(crate) fn invoke_web_cancel(&self, id: u64) {
        if self.shut.get() {
            return;
        }
        let Some(entry) = self.http_cancels.borrow_mut().remove(&id) else {
            return;
        };
        entry.open.set(false);
        let Some(func) = entry.func else {
            return;
        };
        if let Err(err) = self.budget.run(|| func.call::<()>(())) {
            tracing::error!(target: "sixm::script", "[{}] {err}", self.resource);
        }
    }

    fn run_web(
        &self,
        handler: &Function,
        request: u64,
        data: &WebRequest,
        body_handler: &Rc<RefCell<Option<Function>>>,
        open: &Rc<Cell<bool>>,
    ) -> mlua::Result<()> {
        let req = build_request(&self.lua, &self.me, request, data, body_handler, open)?;
        let res = build_response(&self.lua, self.host.clone(), &self.me, request, open)?;
        handler.call::<()>((req, res))?;
        let body = body_handler.borrow_mut().take();
        if let Some(body) = body {
            let text = self.lua.create_string(&data.body)?;
            body.call::<()>(text)?;
        }
        Ok(())
    }
}

/// The `req` table of a request.
fn build_request(
    lua: &Lua,
    weak: &Weak<LuaRuntime>,
    request: u64,
    data: &WebRequest,
    body_handler: &Rc<RefCell<Option<Function>>>,
    open: &Rc<Cell<bool>>,
) -> mlua::Result<Table> {
    let req = lua.create_table()?;
    req.set("address", data.address.as_str())?;
    req.set("method", data.method.as_str())?;
    req.set("path", data.path.as_str())?;
    req.set("query", data.query.as_str())?;
    let headers = lua.create_table()?;
    for (name, value) in &data.headers {
        headers.raw_set(name.as_str(), value.as_str())?;
    }
    req.set("headers", headers)?;
    let slot = Rc::clone(body_handler);
    let set_data =
        lua.create_function(move |_, args: MultiValue| match arguments(args).first() {
            Some(LuaValue::Function(function)) => {
                *slot.borrow_mut() = Some(function.clone());
                Ok(())
            }
            _ => Err(mlua::Error::runtime(
                "setDataHandler needs a function to call with the body",
            )),
        })?;
    req.set("setDataHandler", set_data)?;
    let cancel_open = Rc::clone(open);
    let cancel_weak = weak.clone();
    let set_cancel =
        lua.create_function(move |_, args: MultiValue| match arguments(args).first() {
            Some(LuaValue::Function(function)) => {
                if cancel_open.get()
                    && let Some(rt) = cancel_weak.upgrade().filter(|r| !r.shut.get())
                {
                    let mut cancels = rt.http_cancels.borrow_mut();
                    if let Some(entry) = cancels.get_mut(&request) {
                        entry.func = Some(function.clone());
                    }
                }
                Ok(())
            }
            _ => Err(mlua::Error::runtime(
                "setCancelHandler needs a function to call when the request is canceled",
            )),
        })?;
    req.set("setCancelHandler", set_cancel)?;
    Ok(req)
}

/// The `res` table of a request. Each function sends its step to the host,
/// which keeps the response until it is complete.
fn build_response(
    lua: &Lua,
    host: Weak<dyn Host>,
    weak: &Weak<LuaRuntime>,
    request: u64,
    open: &Rc<Cell<bool>>,
) -> mlua::Result<Table> {
    let res = lua.create_table()?;
    let head_host = host.clone();
    res.set(
        "writeHead",
        lua.create_function(move |_, args: MultiValue| {
            let values = arguments(args);
            let status = status_of(values.first())?;
            let headers = match values.get(1) {
                None | Some(LuaValue::Nil) => Vec::new(),
                Some(LuaValue::Table(table)) => read_headers(table)?,
                Some(other) => {
                    return Err(mlua::Error::runtime(format!(
                        "the headers must be a table, not {}",
                        other.type_name()
                    )));
                }
            };
            reply(&head_host, request, WebReply::Head { status, headers })
        })?,
    )?;
    let write_host = host.clone();
    let write_open = Rc::clone(open);
    let write_weak = weak.clone();
    res.set(
        "write",
        lua.create_function(move |_, args: MultiValue| {
            let data = body_of(arguments(args).first())?;
            if let Err(err) = reply(&write_host, request, WebReply::Write(data)) {
                write_open.set(false);
                if let Some(rt) = write_weak.upgrade() {
                    rt.http_cancels.borrow_mut().remove(&request);
                }
                return Err(err);
            }
            Ok(())
        })?,
    )?;
    let send_open = Rc::clone(open);
    let send_weak = weak.clone();
    res.set(
        "send",
        lua.create_function(move |_, args: MultiValue| {
            let data = body_of(arguments(args).first())?;
            send_open.set(false);
            if let Some(rt) = send_weak.upgrade() {
                rt.http_cancels.borrow_mut().remove(&request);
            }
            reply(&host, request, WebReply::End(data))
        })?,
    )?;
    Ok(res)
}

/// The arguments of a call, without the object itself when it was called with `:`.
fn arguments(args: MultiValue) -> Vec<LuaValue> {
    let mut values: Vec<LuaValue> = args.into_iter().collect();
    if matches!(values.first(), Some(LuaValue::Table(_))) {
        values.remove(0);
    }
    values
}

/// The status code argument of `writeHead`.
fn status_of(value: Option<&LuaValue>) -> mlua::Result<u16> {
    match value {
        Some(LuaValue::Integer(code)) => u16::try_from(*code)
            .ok()
            .filter(|code| (100..=999).contains(code))
            .ok_or_else(|| mlua::Error::runtime(format!("{code} is not an HTTP status code"))),
        _ => Err(mlua::Error::runtime("the status code must be an integer")),
    }
}

/// The body argument of `write` and `send`: a string, or nothing.
fn body_of(value: Option<&LuaValue>) -> mlua::Result<Vec<u8>> {
    match value {
        None | Some(LuaValue::Nil) => Ok(Vec::new()),
        Some(LuaValue::String(text)) => Ok(text.as_bytes().to_vec()),
        Some(other) => Err(mlua::Error::runtime(format!(
            "the response body must be a string, not {}",
            other.type_name()
        ))),
    }
}

/// Passes one step of the answer to the server.
fn reply(host: &Weak<dyn Host>, request: u64, step: WebReply) -> mlua::Result<()> {
    let host = host
        .upgrade()
        .ok_or_else(|| mlua::Error::runtime("the server is no longer available"))?;
    let server = host
        .server()
        .ok_or_else(|| mlua::Error::runtime("HTTP responses need a server"))?;
    server.web_reply(request, step).map_err(lua_err)
}
