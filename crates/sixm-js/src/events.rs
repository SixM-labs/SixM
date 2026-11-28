//! Event handler registry, network-safety checks, cancellation, and dispatch.

use std::collections::{HashMap, HashSet};
use std::rc::Weak;

use rquickjs::function::{Opt, Rest};
use rquickjs::{Ctx, Exception, Function, Object, Persistent, TypedArray, Value as JsValue};
use sixm_core::{EventContext, EventSource, Value};

use crate::runtime::{CALLBACK_BUDGET, JsRuntime};
use crate::sandbox::catch_script;

/// One handler registered with `on` / `AddEventHandler` or `onNet` / `RegisterNetEvent`.
/// A raw handler receives the msgpack bytes of the arguments instead of the values.
struct Handler {
    key: i64,
    func: Persistent<Function<'static>>,
    raw: bool,
}

#[derive(Default)]
pub(crate) struct Events {
    handlers: HashMap<String, Vec<Handler>>,
    net_safe: HashSet<String>,
    last_key: i64,
    /// Cancellation flag of each active dispatch. The top is the innermost one.
    cancel_flags: Vec<bool>,
    /// Cancellation outcome of the last `emit` / `TriggerEvent` call.
    last_emit_canceled: bool,
}

impl Events {
    pub(crate) fn handles(&self, event: &str) -> bool {
        self.handlers
            .get(event)
            .is_some_and(|list| !list.is_empty())
    }
}

/// Installs `on`, `AddEventHandler`, `addEventListener`, `RemoveEventHandler`,
/// `removeEventListener`, `onNet`, `RegisterNetEvent`, `addNetEventListener`,
/// `RegisterServerEvent`, `addRawEventListener`, `addRawEventHandler`, `emit`,
/// `TriggerEvent`, `CancelEvent`, and `WasEventCanceled`.
pub(crate) fn install(ctx: &Ctx<'_>, weak: &Weak<JsRuntime>) -> rquickjs::Result<()> {
    install_listeners(ctx, weak)?;
    install_dispatch(ctx, weak)?;
    Ok(())
}

/// `on` / `AddEventHandler`: attaches a handler that gets the event arguments.
fn add_handler_function<'js>(
    ctx: &Ctx<'js>,
    weak: &Weak<JsRuntime>,
) -> rquickjs::Result<Function<'js>> {
    let weak = weak.clone();
    Function::new(
        ctx.clone(),
        move |ctx: Ctx<'js>, name: String, func: Function<'js>| -> rquickjs::Result<Object<'js>> {
            let rt = JsRuntime::upgrade(&ctx, &weak)?;
            let key = rt.add_handler(&ctx, &name, func, false);
            handle_object(&ctx, &name, key)
        },
    )
}

/// `addEventListener(name, fn, netSafe?)`: like `on`, optionally marking `name` as a network event.
fn add_event_listener_function<'js>(
    ctx: &Ctx<'js>,
    weak: &Weak<JsRuntime>,
) -> rquickjs::Result<Function<'js>> {
    let weak = weak.clone();
    Function::new(
        ctx.clone(),
        move |ctx: Ctx<'js>,
              name: String,
              func: Function<'js>,
              net_safe: Opt<bool>|
              -> rquickjs::Result<Object<'js>> {
            let rt = JsRuntime::upgrade(&ctx, &weak)?;
            if net_safe.0 == Some(true) {
                rt.mark_net_event(&name);
            }
            let key = rt.add_handler(&ctx, &name, func, false);
            handle_object(&ctx, &name, key)
        },
    )
}

/// `addRawEventListener` / `addRawEventHandler`: the handler gets the msgpack bytes and the source.
fn add_raw_listener_function<'js>(
    ctx: &Ctx<'js>,
    weak: &Weak<JsRuntime>,
) -> rquickjs::Result<Function<'js>> {
    let weak = weak.clone();
    Function::new(
        ctx.clone(),
        move |ctx: Ctx<'js>, name: String, func: Function<'js>| -> rquickjs::Result<Object<'js>> {
            let rt = JsRuntime::upgrade(&ctx, &weak)?;
            let key = rt.add_handler(&ctx, &name, func, true);
            handle_object(&ctx, &name, key)
        },
    )
}

fn install_listeners<'js>(ctx: &Ctx<'js>, weak: &Weak<JsRuntime>) -> rquickjs::Result<()> {
    let globals = ctx.globals();

    let add_handler = add_handler_function(ctx, weak)?;
    let add_event_listener = add_event_listener_function(ctx, weak)?;
    let add_raw_event_listener = add_raw_listener_function(ctx, weak)?;
    let remove_handler = {
        let weak = weak.clone();
        Function::new(
            ctx.clone(),
            move |ctx: Ctx<'js>, handle: JsValue<'js>| -> rquickjs::Result<()> {
                let rt = JsRuntime::upgrade(&ctx, &weak)?;
                if let Some(obj) = handle.as_object()
                    && let (Ok(key), Ok(name)) =
                        (obj.get::<_, i64>("key"), obj.get::<_, String>("name"))
                {
                    let removed = rt.remove_handler(&name, key);
                    drop(removed);
                }
                Ok(())
            },
        )?
    };
    let remove_listener = {
        let weak = weak.clone();
        Function::new(
            ctx.clone(),
            move |ctx: Ctx<'js>, name: String, func: Function<'js>| -> rquickjs::Result<()> {
                let rt = JsRuntime::upgrade(&ctx, &weak)?;
                let removed = rt.remove_handler_by_fn(&ctx, &name, func);
                drop(removed);
                Ok(())
            },
        )?
    };
    let register_net_event = {
        let weak = weak.clone();
        Function::new(
            ctx.clone(),
            move |ctx: Ctx<'js>,
                  name: String,
                  func: Opt<Option<Function<'js>>>|
                  -> rquickjs::Result<JsValue<'js>> {
                let rt = JsRuntime::upgrade(&ctx, &weak)?;
                rt.mark_net_event(&name);
                if let Some(func) = func.0.flatten() {
                    let key = rt.add_handler(&ctx, &name, func, false);
                    return handle_object(&ctx, &name, key).map(Object::into_value);
                }
                Ok(JsValue::new_undefined(ctx))
            },
        )?
    };

    for (name, func) in [
        ("on", add_handler.clone()),
        ("AddEventHandler", add_handler),
        ("addEventListener", add_event_listener),
        ("RemoveEventHandler", remove_handler),
        ("removeEventListener", remove_listener),
        ("onNet", register_net_event.clone()),
        ("RegisterNetEvent", register_net_event.clone()),
        ("addNetEventListener", register_net_event.clone()),
        ("RegisterServerEvent", register_net_event),
        ("addRawEventListener", add_raw_event_listener.clone()),
        ("addRawEventHandler", add_raw_event_listener),
    ] {
        globals.set(name, func)?;
    }
    Ok(())
}

fn install_dispatch<'js>(ctx: &Ctx<'js>, weak: &Weak<JsRuntime>) -> rquickjs::Result<()> {
    let globals = ctx.globals();
    let trigger_event = {
        let weak = weak.clone();
        Function::new(
            ctx.clone(),
            move |ctx: Ctx<'js>, name: String, args: Rest<JsValue<'js>>| -> rquickjs::Result<()> {
                let rt = JsRuntime::upgrade(&ctx, &weak)?;
                let values = rt
                    .to_values(&ctx, &args.0)
                    .map_err(|err| Exception::throw_message(&ctx, &err.to_string()))?;
                let host = rt.host(&ctx)?;
                let saved = rt.current_source.get();
                // Run handlers registered on this runtime synchronously using the active `ctx`,
                // then notify all other runtimes through `host.trigger_event`.
                let local_ctx = EventContext::invoked_by(rt.resource.clone());
                rt.dispatch_event_in_ctx(&ctx, &name, EventSource::Local, &values, &local_ctx);
                let prev_skip = rt.skip_self_event.replace(true);
                let outcome =
                    host.trigger_event(&name, EventSource::Local, &values, Some(&rt.resource));
                rt.skip_self_event.set(prev_skip);
                let _ = JsRuntime::set_source_global(&ctx, saved);
                rt.current_source.set(saved);
                let host_canceled =
                    outcome.map_err(|err| Exception::throw_message(&ctx, &err.to_string()))?;
                rt.events.borrow_mut().last_emit_canceled =
                    local_ctx.is_canceled() || host_canceled;
                Ok(())
            },
        )?
    };
    let cancel_event = {
        let weak = weak.clone();
        Function::new(ctx.clone(), move |ctx: Ctx<'js>| -> rquickjs::Result<()> {
            let rt = JsRuntime::upgrade(&ctx, &weak)?;
            rt.set_canceled();
            Ok(())
        })?
    };
    let was_canceled = {
        let weak = weak.clone();
        Function::new(
            ctx.clone(),
            move |ctx: Ctx<'js>| -> rquickjs::Result<bool> {
                let rt = JsRuntime::upgrade(&ctx, &weak)?;
                Ok(rt.was_canceled())
            },
        )?
    };

    for (name, func) in [
        ("emit", trigger_event.clone()),
        ("TriggerEvent", trigger_event),
        ("CancelEvent", cancel_event),
        ("WasEventCanceled", was_canceled),
    ] {
        globals.set(name, func)?;
    }
    Ok(())
}

fn handle_object<'js>(ctx: &Ctx<'js>, event: &str, key: i64) -> rquickjs::Result<Object<'js>> {
    let obj = Object::new(ctx.clone())?;
    obj.set("key", key)?;
    obj.set("name", event)?;
    Ok(obj)
}

impl JsRuntime {
    fn add_handler<'js>(&self, ctx: &Ctx<'js>, event: &str, func: Function<'js>, raw: bool) -> i64 {
        let mut events = self.events.borrow_mut();
        events.last_key += 1;
        let key = events.last_key;
        events
            .handlers
            .entry(event.to_owned())
            .or_default()
            .push(Handler {
                key,
                func: Persistent::save(ctx, func),
                raw,
            });
        key
    }

    fn remove_handler(&self, event: &str, key: i64) -> Option<Persistent<Function<'static>>> {
        let mut events = self.events.borrow_mut();
        let list = events.handlers.get_mut(event)?;
        let index = list.iter().position(|handler| handler.key == key)?;
        let removed = list.remove(index).func;
        if list.is_empty() {
            events.handlers.remove(event);
        }
        Some(removed)
    }

    fn remove_handler_by_fn<'js>(
        &self,
        ctx: &Ctx<'js>,
        event: &str,
        func: Function<'js>,
    ) -> Option<Persistent<Function<'static>>> {
        let needle = Persistent::save(ctx, func);
        let mut events = self.events.borrow_mut();
        let list = events.handlers.get_mut(event)?;
        let index = list.iter().position(|handler| handler.func == needle)?;
        let removed = list.remove(index).func;
        if list.is_empty() {
            events.handlers.remove(event);
        }
        Some(removed)
    }

    pub(crate) fn mark_net_event(&self, event: &str) {
        self.events.borrow_mut().net_safe.insert(event.to_owned());
    }

    pub(crate) fn set_canceled(&self) {
        if let Some(flag) = self.events.borrow_mut().cancel_flags.last_mut() {
            *flag = true;
        }
    }

    pub(crate) fn was_canceled(&self) -> bool {
        let events = self.events.borrow();
        events
            .cancel_flags
            .last()
            .copied()
            .unwrap_or(events.last_emit_canceled)
    }

    /// Runs every handler of `event` synchronously up to their first suspension.
    pub(crate) fn dispatch_event(
        &self,
        event: &str,
        source: EventSource,
        args: &[Value],
        ctx: &EventContext,
    ) {
        if self.skip_self_event.get() {
            return;
        }
        if self.in_ctx.get() {
            self.pending_events.borrow_mut().push_back((
                event.to_owned(),
                source,
                args.to_vec(),
                ctx.invoker().cloned(),
            ));
            return;
        }
        self.with_ctx(|js_ctx| {
            self.dispatch_event_in_ctx(js_ctx, event, source, args, ctx);
        });
    }

    pub(crate) fn dispatch_event_in_ctx(
        &self,
        js_ctx: &Ctx<'_>,
        event: &str,
        source: EventSource,
        args: &[Value],
        ctx: &EventContext,
    ) {
        let network = matches!(source, EventSource::Net(_) | EventSource::Server);
        if network && !self.events.borrow().net_safe.contains(event) {
            tracing::warn!(
                target: "sixm::script",
                "[{}] dropped network event `{event}`: it was not registered with RegisterNetEvent",
                self.resource
            );
            return;
        }
        let handlers: Vec<(Persistent<Function<'static>>, bool)> = self
            .events
            .borrow()
            .handlers
            .get(event)
            .map(|list| list.iter().map(|h| (h.func.clone(), h.raw)).collect())
            .unwrap_or_default();
        if handlers.is_empty() {
            return;
        }
        // The msgpack array of the arguments, packed once for all raw handlers.
        let raw_payload = if handlers.iter().any(|(_, raw)| *raw) {
            match sixm_core::msgpack::pack(&Value::Array(args.to_vec())) {
                Ok(bytes) => Some(bytes),
                Err(err) => {
                    tracing::error!(
                        target: "sixm::script",
                        "[{}] raw event `{event}`: {err}",
                        self.resource
                    );
                    None
                }
            }
        } else {
            None
        };

        let outer_source = self.current_source.replace(source);
        let outer_invoker = std::mem::replace(
            &mut *self.current_invoker.borrow_mut(),
            ctx.invoker().cloned(),
        );
        self.events
            .borrow_mut()
            .cancel_flags
            .push(ctx.is_canceled());

        for (handler, raw) in handlers {
            let raw_bytes = match (raw, &raw_payload) {
                (false, _) => None,
                (true, Some(bytes)) => Some(bytes),
                (true, None) => continue,
            };
            self.handled_events
                .set(self.handled_events.get().saturating_add(1));
            let _budget = self.enter_budget(CALLBACK_BUDGET);
            let outcome = (|| -> Result<(), sixm_core::ScriptError> {
                Self::set_source_global(js_ctx, source)
                    .map_err(|err| crate::runtime::js_err(js_ctx, err))?;
                let func = handler
                    .restore(js_ctx)
                    .map_err(|err| crate::runtime::js_err(js_ctx, err))?;
                let js_args = match raw_bytes {
                    Some(bytes) => {
                        let payload = TypedArray::<u8>::new_copy(js_ctx.clone(), bytes)
                            .map(TypedArray::into_value)
                            .map_err(|err| crate::runtime::js_err(js_ctx, err))?;
                        let source_value = js_ctx
                            .globals()
                            .get::<_, JsValue>("source")
                            .map_err(|err| crate::runtime::js_err(js_ctx, err))?;
                        vec![payload, source_value]
                    }
                    None => self.values_to_js(js_ctx, args)?,
                };
                let mut call_args = rquickjs::function::Args::new(js_ctx.clone(), js_args.len());
                call_args
                    .push_args(js_args)
                    .map_err(|err| crate::runtime::js_err(js_ctx, err))?;
                catch_script(js_ctx, func.call_arg::<()>(call_args))
            })();
            if let Err(err) = outcome {
                tracing::error!(target: "sixm::script", "[{}] {err}", self.resource);
            }
            self.drain_jobs();
            if self.was_canceled() {
                ctx.cancel();
            }
        }

        self.events.borrow_mut().cancel_flags.pop();
        let _ = Self::set_source_global(js_ctx, outer_source);
        *self.current_invoker.borrow_mut() = outer_invoker;
        self.current_source.set(outer_source);
    }
}
