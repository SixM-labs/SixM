//! Host, convar, command, file, hash, and timer natives, plus wiring all
//! side-common and side-specific globals onto `globalThis`.

use std::path::PathBuf;
use std::rc::{Rc, Weak};

use rquickjs::function::Opt;
use rquickjs::{Ctx, Exception, Function, Object, Persistent, Value as JsValue};
use sixm_core::{ConvarKind, convar_filter_matches, game_timer_ms, joaat, normalize_resource_path};

use crate::client;
use crate::convert::{bytes_to_js, read_bytes};
use crate::entities;
use crate::events;
use crate::exports;
use crate::http;
use crate::kvp;
use crate::players::{self, server};
use crate::resources;
use crate::runtime::{CALLBACK_BUDGET, JsRuntime, Side};
use crate::sandbox::{self, catch_script};
use crate::scheduler;
use crate::state;
use crate::vector;
use crate::web;

/// Installs every global function, `Citizen`, `exports`, `console`, `GlobalState`, etc.
pub(crate) fn install(runtime: &Rc<JsRuntime>) -> Result<(), sixm_core::ScriptError> {
    let _ = game_timer_ms();
    let weak = &runtime.me;
    let outcome = runtime.ctx.with(|ctx| {
        sandbox::install_console(&ctx, weak, &runtime.resource)?;
        ctx.globals().set("source", "")?;
        scheduler::install(&ctx, weak)?;
        events::install(&ctx, weak)?;
        install_host(&ctx, weak, runtime.side)?;
        entities::install(&ctx, weak)?;
        kvp::install(&ctx, weak)?;
        match runtime.side {
            Side::Server => {
                players::install(&ctx, weak)?;
                entities::install_server(&ctx, weak)?;
                resources::install_server(&ctx, weak)?;
                http::install(&ctx, weak)?;
                web::install(&ctx, weak)?;
                install_server_convars(&ctx, weak)?;
            }
            Side::Client { local_player } => {
                client::install(&ctx, weak, local_player)?;
                entities::install_client(&ctx, weak)?;
            }
        }
        state::install(&ctx, weak)?;
        resources::install(&ctx, weak)?;
        vector::install(&ctx)?;
        exports::install(&ctx, weak)?;
        Ok::<(), rquickjs::Error>(())
    });
    runtime.ctx.with(|ctx| catch_script(&ctx, outcome))
}

fn install_host(ctx: &Ctx<'_>, weak: &Weak<JsRuntime>, side: Side) -> rquickjs::Result<()> {
    install_resource_files(ctx, weak)?;
    install_convars_and_commands(ctx, weak)?;
    install_convar_listeners(ctx, weak)?;
    install_misc(ctx, weak, side)?;
    Ok(())
}

fn install_resource_files<'js>(ctx: &Ctx<'js>, weak: &Weak<JsRuntime>) -> rquickjs::Result<()> {
    let globals = ctx.globals();
    {
        let weak = weak.clone();
        globals.set(
            "GetCurrentResourceName",
            Function::new(
                ctx.clone(),
                move |ctx: Ctx<'js>| -> rquickjs::Result<String> {
                    let rt = JsRuntime::upgrade(&ctx, &weak)?;
                    Ok(rt.resource.as_ref().to_owned())
                },
            )?,
        )?;
    }
    {
        let weak = weak.clone();
        globals.set(
            "GetResourcePath",
            Function::new(
                ctx.clone(),
                move |ctx: Ctx<'js>, name: String| -> rquickjs::Result<Option<String>> {
                    let rt = JsRuntime::upgrade(&ctx, &weak)?;
                    let root = resource_root(&rt, &ctx, &name)?;
                    Ok(root.map(|path| path.to_string_lossy().into_owned()))
                },
            )?,
        )?;
    }
    {
        let weak = weak.clone();
        globals.set(
            "LoadResourceFile",
            Function::new(
                ctx.clone(),
                move |ctx: Ctx<'js>,
                      resource: String,
                      path: String|
                      -> rquickjs::Result<JsValue<'js>> {
                    let rt = JsRuntime::upgrade(&ctx, &weak)?;
                    let Some(normalized) = normalize_resource_path(&path) else {
                        return Ok(JsValue::new_null(ctx));
                    };
                    match rt.host(&ctx)?.load_resource_file(&resource, &normalized) {
                        Some(bytes) => bytes_to_js(&ctx, &bytes)
                            .map_err(|err| Exception::throw_message(&ctx, &err.to_string())),
                        None => Ok(JsValue::new_null(ctx)),
                    }
                },
            )?,
        )?;
    }
    {
        let weak = weak.clone();
        globals.set(
            "SaveResourceFile",
            Function::new(
                ctx.clone(),
                move |ctx: Ctx<'js>,
                      resource: String,
                      path: String,
                      data: JsValue<'js>|
                      -> rquickjs::Result<bool> {
                    let rt = JsRuntime::upgrade(&ctx, &weak)?;
                    if resource != rt.resource.as_ref() {
                        return Ok(false);
                    }
                    let Some(normalized) = normalize_resource_path(&path) else {
                        return Ok(false);
                    };
                    let bytes = data_bytes(&ctx, &data)?;
                    Ok(rt
                        .host(&ctx)?
                        .save_resource_file(&resource, &normalized, &bytes))
                },
            )?,
        )?;
    }
    Ok(())
}

fn install_convars_and_commands<'js>(
    ctx: &Ctx<'js>,
    weak: &Weak<JsRuntime>,
) -> rquickjs::Result<()> {
    let globals = ctx.globals();
    {
        let weak = weak.clone();
        globals.set(
            "GetConvar",
            Function::new(
                ctx.clone(),
                move |ctx: Ctx<'js>,
                      name: String,
                      default: JsValue<'js>|
                      -> rquickjs::Result<JsValue<'js>> {
                    let rt = JsRuntime::upgrade(&ctx, &weak)?;
                    match rt.host(&ctx)?.get_resource_convar(&rt.resource, &name) {
                        Some(value) => rquickjs::String::from_str(ctx, &value)
                            .map(rquickjs::String::into_value),
                        None => Ok(default),
                    }
                },
            )?,
        )?;
    }
    {
        let weak = weak.clone();
        globals.set(
            "GetConvarInt",
            Function::new(
                ctx.clone(),
                move |ctx: Ctx<'js>, name: String, default: i64| -> rquickjs::Result<i64> {
                    let rt = JsRuntime::upgrade(&ctx, &weak)?;
                    let parsed = rt
                        .host(&ctx)?
                        .get_resource_convar(&rt.resource, &name)
                        .and_then(|value| value.trim().parse::<i64>().ok());
                    Ok(parsed.unwrap_or(default))
                },
            )?,
        )?;
    }
    {
        let weak = weak.clone();
        globals.set(
            "GetConvarBool",
            Function::new(
                ctx.clone(),
                move |ctx: Ctx<'js>, name: String, default: bool| -> rquickjs::Result<bool> {
                    let rt = JsRuntime::upgrade(&ctx, &weak)?;
                    let parsed = rt
                        .host(&ctx)?
                        .get_resource_convar(&rt.resource, &name)
                        .and_then(|value| parse_bool(&value));
                    Ok(parsed.unwrap_or(default))
                },
            )?,
        )?;
    }
    {
        let weak = weak.clone();
        globals.set(
            "ExecuteCommand",
            Function::new(
                ctx.clone(),
                move |ctx: Ctx<'js>, line: String| -> rquickjs::Result<()> {
                    let rt = JsRuntime::upgrade(&ctx, &weak)?;
                    rt.host(&ctx)?.execute_command(&line, Some(&rt.resource));
                    Ok(())
                },
            )?,
        )?;
    }
    {
        let weak = weak.clone();
        globals.set(
            "RegisterCommand",
            Function::new(
                ctx.clone(),
                move |ctx: Ctx<'js>,
                      name: String,
                      func: Function<'js>,
                      restricted: Opt<Option<bool>>|
                      -> rquickjs::Result<()> {
                    let rt = JsRuntime::upgrade(&ctx, &weak)?;
                    let reference = rt
                        .pin_function(&ctx, func)
                        .map_err(|err| Exception::throw_message(&ctx, &err.to_string()))?;
                    rt.host(&ctx)?.register_command(
                        &name,
                        reference,
                        restricted.0.flatten().unwrap_or(false),
                    );
                    Ok(())
                },
            )?,
        )?;
    }
    Ok(())
}

fn install_convar_listeners<'js>(ctx: &Ctx<'js>, weak: &Weak<JsRuntime>) -> rquickjs::Result<()> {
    let globals = ctx.globals();
    {
        let weak = weak.clone();
        globals.set(
            "AddConvarChangeListener",
            Function::new(
                ctx.clone(),
                move |ctx: Ctx<'js>,
                      filter: JsValue<'js>,
                      func: Function<'js>|
                      -> rquickjs::Result<i64> {
                    let rt = JsRuntime::upgrade(&ctx, &weak)?;
                    let pattern = if filter.is_null() || filter.is_undefined() {
                        String::new()
                    } else if let Some(text) = filter.as_string() {
                        text.to_string()?
                    } else {
                        return Err(Exception::throw_type(
                            &ctx,
                            "AddConvarChangeListener expects a string or null filter",
                        ));
                    };
                    Ok(rt.convar_listeners.borrow_mut().add(&ctx, pattern, func))
                },
            )?,
        )?;
    }
    {
        let weak = weak.clone();
        globals.set(
            "RemoveConvarChangeListener",
            Function::new(
                ctx.clone(),
                move |ctx: Ctx<'js>, cookie: i64| -> rquickjs::Result<()> {
                    let rt = JsRuntime::upgrade(&ctx, &weak)?;
                    let removed = rt.convar_listeners.borrow_mut().remove(cookie);
                    drop(removed);
                    Ok(())
                },
            )?,
        )?;
    }
    {
        let weak = weak.clone();
        globals.set(
            "GetRegisteredCommands",
            Function::new(
                ctx.clone(),
                move |ctx: Ctx<'js>| -> rquickjs::Result<Vec<Object<'js>>> {
                    let rt = JsRuntime::upgrade(&ctx, &weak)?;
                    let names = rt.host(&ctx)?.registered_commands();
                    let mut out = Vec::with_capacity(names.len());
                    for name in names {
                        let obj = Object::new(ctx.clone())?;
                        obj.set("name", name)?;
                        out.push(obj);
                    }
                    Ok(out)
                },
            )?,
        )?;
    }
    Ok(())
}

fn install_misc<'js>(ctx: &Ctx<'js>, weak: &Weak<JsRuntime>, side: Side) -> rquickjs::Result<()> {
    let globals = ctx.globals();
    globals.set(
        "IsDuplicityVersion",
        Function::new(ctx.clone(), move || side == Side::Server)?,
    )?;
    {
        let weak = weak.clone();
        globals.set(
            "GetGameTimer",
            Function::new(ctx.clone(), move |ctx: Ctx<'js>| -> rquickjs::Result<i64> {
                let _ = JsRuntime::upgrade(&ctx, &weak)?;
                Ok(game_timer_ms())
            })?,
        )?;
    }
    globals.set(
        "GetHashKey",
        Function::new(ctx.clone(), |text: String| hash_key(text.as_bytes()))?,
    )?;
    Ok(())
}

fn install_server_convars<'js>(ctx: &Ctx<'js>, weak: &Weak<JsRuntime>) -> rquickjs::Result<()> {
    let globals = ctx.globals();
    let setters = [
        ("SetConvar", ConvarKind::Plain),
        ("SetConvarReplicated", ConvarKind::Replicated),
        ("SetConvarServerInfo", ConvarKind::ServerInfo),
    ];
    for (name, kind) in setters {
        let weak = weak.clone();
        let setter = Function::new(
            ctx.clone(),
            move |ctx: Ctx<'js>, convar: String, value: String| -> rquickjs::Result<()> {
                let rt = JsRuntime::upgrade(&ctx, &weak)?;
                server(&ctx, &*rt.host(&ctx)?)?
                    .set_convar(&convar, &value, kind)
                    .map_err(|err| Exception::throw_message(&ctx, &err.to_string()))
            },
        )?;
        globals.set(name, setter)?;
    }
    Ok(())
}

/// Extracts raw bytes from a JS `string`, `Uint8Array`, or `ArrayBuffer`.
pub(crate) fn data_bytes(ctx: &Ctx<'_>, value: &JsValue<'_>) -> rquickjs::Result<Vec<u8>> {
    if let Some(text) = value.as_string() {
        return text.to_string().map(String::into_bytes);
    }
    if let Some(obj) = value.as_object()
        && let Some(bytes) =
            read_bytes(obj).map_err(|err| Exception::throw_message(ctx, &err.to_string()))?
    {
        return Ok(bytes);
    }
    Err(Exception::throw_type(
        ctx,
        "expected a string, Uint8Array, or ArrayBuffer",
    ))
}

fn parse_bool(text: &str) -> Option<bool> {
    match text.trim().to_ascii_lowercase().as_str() {
        "true" | "1" | "yes" | "on" => Some(true),
        "false" | "0" | "no" | "off" => Some(false),
        _ => None,
    }
}

#[expect(
    clippy::cast_possible_wrap,
    reason = "GetHashKey returns the joaat hash reinterpreted as signed 32-bit"
)]
fn hash_key(text: &[u8]) -> i64 {
    i64::from(joaat(text) as i32)
}

fn resource_root(rt: &JsRuntime, ctx: &Ctx<'_>, name: &str) -> rquickjs::Result<Option<PathBuf>> {
    if name == rt.resource.as_ref() {
        return Ok(Some(rt.root.clone()));
    }
    Ok(rt.host(ctx)?.resource_path(name))
}

struct ConvarListener {
    cookie: i64,
    filter: String,
    func: Persistent<Function<'static>>,
}

#[derive(Default)]
pub(crate) struct ConvarListeners {
    listeners: Vec<ConvarListener>,
    last_cookie: i64,
}

impl ConvarListeners {
    fn add<'js>(&mut self, ctx: &Ctx<'js>, filter: String, func: Function<'js>) -> i64 {
        self.last_cookie += 1;
        let cookie = self.last_cookie;
        self.listeners.push(ConvarListener {
            cookie,
            filter,
            func: Persistent::save(ctx, func),
        });
        cookie
    }

    fn remove(&mut self, cookie: i64) -> Option<Persistent<Function<'static>>> {
        let idx = self.listeners.iter().position(|l| l.cookie == cookie)?;
        Some(self.listeners.remove(idx).func)
    }

    fn matching(&self, name: &str) -> Vec<Persistent<Function<'static>>> {
        self.listeners
            .iter()
            .filter(|l| convar_filter_matches(&l.filter, name))
            .map(|l| l.func.clone())
            .collect()
    }
}

impl JsRuntime {
    pub(crate) fn dispatch_convar_change(&self, name: &str, value: Option<&str>, replicated: bool) {
        if self.in_ctx.get() {
            self.pending_convar_changes.borrow_mut().push_back((
                name.to_owned(),
                value.map(str::to_owned),
                replicated,
            ));
            return;
        }
        self.with_ctx(|ctx| {
            self.dispatch_convar_change_in_ctx(ctx, name);
        });
    }

    fn dispatch_convar_change_in_ctx(&self, ctx: &Ctx<'_>, name: &str) {
        let functions = self.convar_listeners.borrow().matching(name);
        if functions.is_empty() {
            return;
        }
        let outer_source = self.current_source.get();
        let outer_invoker = self.current_invoker();

        for func in functions {
            let _budget = self.enter_budget(CALLBACK_BUDGET);
            let outcome = (|| -> Result<(), sixm_core::ScriptError> {
                let func = func
                    .restore(ctx)
                    .map_err(|err| crate::runtime::js_err(ctx, err))?;
                catch_script(ctx, func.call::<_, ()>((name, "")))
            })();
            if let Err(err) = outcome {
                tracing::error!(
                    target: "sixm::script",
                    "[{}] convar change of `{name}`: {err}",
                    self.resource
                );
            }
            self.drain_jobs();
        }

        let _ = Self::set_source_global(ctx, outer_source);
        *self.current_invoker.borrow_mut() = outer_invoker;
        self.current_source.set(outer_source);
    }
}
