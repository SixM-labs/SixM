//! Event handler registry, network-safety checks, and dispatch.

use std::collections::{HashMap, HashSet};
use std::time::Instant;

use mlua::{Function, MultiValue, Value as LuaValue};
use sixm_core::{EventContext, EventSource, ScriptError, Value};

use crate::runtime::{IntoScriptResult, LuaRuntime};

/// One handler registered with `AddEventHandler` or `RegisterNetEvent`.
struct Handler {
    key: i64,
    func: Function,
}

/// Event state. Lives in a `RefCell` and is never borrowed across a Lua call.
#[derive(Default)]
pub(crate) struct Events {
    handlers: HashMap<String, Vec<Handler>>,
    net_safe: HashSet<String>,
    last_key: i64,
    /// Cancellation flag of each active dispatch. The top is the innermost one.
    cancel_flags: Vec<bool>,
    /// Cancellation outcome of the last `TriggerEvent` call.
    last_emit_canceled: bool,
}

impl Events {
    /// Whether at least one handler is registered for `event`.
    pub(crate) fn handles(&self, event: &str) -> bool {
        self.handlers
            .get(event)
            .is_some_and(|list| !list.is_empty())
    }
}

impl LuaRuntime {
    /// Registers a handler and returns its key.
    pub(crate) fn add_handler(&self, event: &str, func: Function) -> i64 {
        let mut events = self.events.borrow_mut();
        events.last_key += 1;
        let key = events.last_key;
        events
            .handlers
            .entry(event.to_owned())
            .or_default()
            .push(Handler { key, func });
        key
    }

    /// Removes the handler with `key`. The removed function is returned so the
    /// caller drops it outside the borrow.
    pub(crate) fn remove_handler(&self, event: &str, key: i64) -> Option<Function> {
        let mut events = self.events.borrow_mut();
        let list = events.handlers.get_mut(event)?;
        let index = list.iter().position(|handler| handler.key == key)?;
        let removed = list.remove(index).func;
        if list.is_empty() {
            events.handlers.remove(event);
        }
        Some(removed)
    }

    /// Marks `event` as safe to receive from network sources.
    pub(crate) fn mark_net_event(&self, event: &str) {
        self.events.borrow_mut().net_safe.insert(event.to_owned());
    }

    /// Sets the cancellation flag of the innermost dispatch.
    pub(crate) fn set_canceled(&self) {
        if let Some(flag) = self.events.borrow_mut().cancel_flags.last_mut() {
            *flag = true;
        }
    }

    /// Records the outcome of a local `TriggerEvent` dispatch.
    pub(crate) fn set_last_emit_canceled(&self, canceled: bool) {
        self.events.borrow_mut().last_emit_canceled = canceled;
    }

    /// Reads the cancellation flag of the innermost dispatch, or the last `TriggerEvent`.
    pub(crate) fn was_canceled(&self) -> bool {
        let events = self.events.borrow();
        events
            .cancel_flags
            .last()
            .copied()
            .unwrap_or(events.last_emit_canceled)
    }

    /// Runs every handler of `event`. Each handler starts as its own task and
    /// runs until its first yield.
    pub(crate) fn dispatch_event(
        &self,
        event: &str,
        source: EventSource,
        args: &[Value],
        ctx: &EventContext,
    ) -> Result<(), ScriptError> {
        let network = matches!(source, EventSource::Net(_) | EventSource::Server);
        if network && !self.events.borrow().net_safe.contains(event) {
            tracing::warn!(
                target: "sixm::script",
                "[{}] dropped network event `{event}`: it was not registered with RegisterNetEvent",
                self.resource
            );
            return Ok(());
        }
        let handlers = self.handler_functions(event);
        if handlers.is_empty() {
            return Ok(());
        }
        let lua_args = self.to_lua_multi(args).into_script()?;

        self.events
            .borrow_mut()
            .cancel_flags
            .push(ctx.is_canceled());
        let outcome = self.run_handlers(handlers, source, &lua_args, ctx);
        self.events.borrow_mut().cancel_flags.pop();
        outcome.into_script()
    }

    fn handler_functions(&self, event: &str) -> Vec<Function> {
        self.events
            .borrow()
            .handlers
            .get(event)
            .map(|list| list.iter().map(|handler| handler.func.clone()).collect())
            .unwrap_or_default()
    }

    fn run_handlers(
        &self,
        handlers: Vec<Function>,
        source: EventSource,
        args: &MultiValue,
        ctx: &EventContext,
    ) -> mlua::Result<()> {
        for handler in handlers {
            let (slot, _) = self.spawn(handler, source, ctx.invoker().cloned())?;
            self.handled_events
                .set(self.handled_events.get().saturating_add(1));
            self.run(slot, Instant::now(), args.clone());
            if self.was_canceled() {
                ctx.cancel();
            }
        }
        Ok(())
    }
}

/// Builds the `{ key, name }` table that `AddEventHandler` returns and `RemoveEventHandler` reads.
pub(crate) fn handle_table(
    runtime: &LuaRuntime,
    event: &str,
    key: i64,
) -> mlua::Result<mlua::Table> {
    let table = runtime.lua.create_table()?;
    table.raw_set("key", LuaValue::Integer(key))?;
    table.raw_set("name", event)?;
    Ok(table)
}
