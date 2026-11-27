//! `QuickJS` runtime & context construction, sandbox limits, console logging,
//! and JavaScript error/stack formatting.

use std::cell::Cell;
use std::rc::{Rc, Weak};
use std::time::Instant;

use rquickjs::context::intrinsic;
use rquickjs::function::Rest;
use rquickjs::{CatchResultExt, CaughtError, Context, Ctx, Exception, Function, Runtime, Value};
use sixm_core::{ResourceName, ScriptError};

use crate::runtime::JsRuntime;

/// Heap limit per JS resource instance (64 MiB).
pub(crate) const MEMORY_LIMIT: usize = 64 * 1024 * 1024;
/// Call stack limit per JS resource instance (256 KiB).
pub(crate) const MAX_STACK_SIZE: usize = 256 * 1024;

/// Creates an isolated `QuickJS` runtime with memory, stack, and CPU interrupt limits.
pub(crate) fn new_runtime(
    budget_deadline: Rc<Cell<Option<Instant>>>,
) -> Result<Runtime, ScriptError> {
    let rt = Runtime::new().map_err(|err| ScriptError::Runtime(err.to_string()))?;
    rt.set_memory_limit(MEMORY_LIMIT);
    rt.set_max_stack_size(MAX_STACK_SIZE);
    rt.set_interrupt_handler(Some(Box::new(move || {
        budget_deadline
            .get()
            .is_some_and(|limit| Instant::now() >= limit)
    })));
    Ok(rt)
}

/// Creates a sandboxed context without `eval` / `Function(...)`, `SharedArrayBuffer`, or `Atomics`.
pub(crate) fn new_context(rt: &Runtime) -> Result<Context, ScriptError> {
    let ctx = Context::builder()
        .with::<intrinsic::Date>()
        .with::<intrinsic::Eval>()
        .with::<intrinsic::RegExpCompiler>()
        .with::<intrinsic::RegExp>()
        .with::<intrinsic::Json>()
        .with::<intrinsic::Proxy>()
        .with::<intrinsic::MapSet>()
        .with::<intrinsic::TypedArrays>()
        .with::<intrinsic::Promise>()
        .with::<intrinsic::WeakRef>()
        .build(rt)
        .map_err(|err| ScriptError::Runtime(err.to_string()))?;

    ctx.with(|ctx| {
        let globals = ctx.globals();
        for dangerous in ["SharedArrayBuffer", "Atomics", "eval"] {
            let _ = globals.remove(dangerous);
        }
        // Disable dynamic code generation via `Function(...)` / `(fn).constructor(...)`
        // while keeping `Function.prototype` (`call`, `apply`, `bind`, `instanceof`) intact.
        let blocked = Function::new(ctx.clone(), |ctx: Ctx<'_>| -> rquickjs::Result<()> {
            Err(Exception::throw_type(&ctx, "Function constructor is disabled"))
        })?;
        let fn_ctor: rquickjs::Object<'_> = globals.get("Function")?;
        let fn_proto: Value<'_> = fn_ctor.get("prototype")?;
        blocked.set("prototype", fn_proto.clone())?;
        if let Some(proto_obj) = fn_proto.as_object() {
            proto_obj.set("constructor", blocked.clone())?;
        }
        globals.set("Function", blocked)?;
        Ok::<(), rquickjs::Error>(())
    })
    .map_err(|err| ScriptError::Runtime(err.to_string()))?;

    Ok(ctx)
}

/// Installs `print` and `console` (`log`, `info`, `warn`, `error`, `debug`) on `globalThis`.
pub(crate) fn install_console(
    ctx: &Ctx<'_>,
    weak: &Weak<JsRuntime>,
    resource: &ResourceName,
) -> rquickjs::Result<()> {
    let globals = ctx.globals();

    let print_fn = make_logger(ctx, weak, resource.clone(), LogLevel::Info)?;
    globals.set("print", print_fn)?;

    let console = rquickjs::Object::new(ctx.clone())?;
    let levels = [
        ("log", LogLevel::Info),
        ("info", LogLevel::Info),
        ("warn", LogLevel::Warn),
        ("error", LogLevel::Error),
        ("debug", LogLevel::Debug),
    ];
    for (name, level) in levels {
        console.set(name, make_logger(ctx, weak, resource.clone(), level)?)?;
    }
    globals.set("console", console)?;
    Ok(())
}

#[derive(Clone, Copy)]
enum LogLevel {
    Info,
    Warn,
    Error,
    Debug,
}

fn make_logger<'js>(
    ctx: &Ctx<'js>,
    weak: &Weak<JsRuntime>,
    resource: ResourceName,
    level: LogLevel,
) -> rquickjs::Result<Function<'js>> {
    let weak = weak.clone();
    Function::new(
        ctx.clone(),
        move |ctx: Ctx<'js>, args: Rest<Value<'js>>| -> rquickjs::Result<()> {
            let _ = JsRuntime::upgrade(&ctx, &weak)?;
            let mut parts = Vec::with_capacity(args.0.len());
            for val in &args.0 {
                parts.push(format_js_value(&ctx, val));
            }
            let line = parts.join(" ");
            match level {
                LogLevel::Info => tracing::info!(target: "sixm::script", "[{resource}] {line}"),
                LogLevel::Warn => tracing::warn!(target: "sixm::script", "[{resource}] {line}"),
                LogLevel::Error => tracing::error!(target: "sixm::script", "[{resource}] {line}"),
                LogLevel::Debug => tracing::debug!(target: "sixm::script", "[{resource}] {line}"),
            }
            Ok(())
        },
    )
}

/// Converts a JS value into a human-readable string without throwing.
pub(crate) fn format_js_value<'js>(ctx: &Ctx<'js>, val: &Value<'js>) -> String {
    if val.is_undefined() {
        return "undefined".to_owned();
    }
    if val.is_null() {
        return "null".to_owned();
    }
    if let Some(text) = val.as_string().and_then(|s| s.to_string().ok()) {
        return text;
    }
    if let Some(obj) = val.as_object()
        && let Some(exc) = Exception::from_object(obj.clone())
    {
        return format_exception(&exc);
    }
    if let Ok(string_ctor) = ctx.globals().get::<_, Function<'js>>("String")
        && let Ok(text) = string_ctor.call::<_, String>((val.clone(),))
    {
        return text;
    }
    format!("[{}]", val.type_name())
}

/// Formats an `Exception` including its `name`, `message`, and `stack`.
pub(crate) fn format_exception(exc: &Exception<'_>) -> String {
    let name = exc
        .as_object()
        .get::<_, Option<String>>("name")
        .ok()
        .flatten()
        .unwrap_or_else(|| "Error".to_owned());
    let message = exc.message().unwrap_or_default();
    let header = if message.is_empty() {
        name
    } else {
        format!("{name}: {message}")
    };
    match exc.stack() {
        Some(stack) if !stack.is_empty() => format!("{header}\n{stack}"),
        _ => header,
    }
}

/// Formats a [`CaughtError`] into a readable string, clearing any pending exception.
pub(crate) fn format_caught<'js>(ctx: &Ctx<'js>, err: CaughtError<'js>) -> String {
    match err {
        CaughtError::Exception(exc) => format_exception(&exc),
        CaughtError::Value(val) => format_js_value(ctx, &val),
        CaughtError::Error(err) => err.to_string(),
    }
}

/// Formats a raw `Value` caught directly from `ctx.catch()`.
pub(crate) fn format_caught_value<'js>(ctx: &Ctx<'js>, val: &Value<'js>) -> String {
    if let Some(obj) = val.as_object()
        && let Some(exc) = Exception::from_object(obj.clone())
    {
        format_exception(&exc)
    } else {
        format_js_value(ctx, val)
    }
}

/// Catches any `QuickJS` exception on `ctx` and maps it into [`ScriptError::Runtime`].
pub(crate) fn catch_script<T>(ctx: &Ctx<'_>, result: rquickjs::Result<T>) -> Result<T, ScriptError> {
    result
        .catch(ctx)
        .map_err(|caught| ScriptError::Runtime(format_caught(ctx, caught)))
}
