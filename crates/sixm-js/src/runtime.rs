//! The [`JsRuntime`] type: one isolated `QuickJS` runtime and context per resource instance.

use std::cell::{Cell, RefCell};
use std::collections::{HashSet, VecDeque};
use std::fmt;
use std::path::{Path, PathBuf};
use std::rc::{Rc, Weak};
use std::time::{Duration, Instant};

use rquickjs::context::EvalOptions;
use rquickjs::{
    Atom, Context, Ctx, Exception, Filter, Persistent, Runtime, Symbol, Value as JsValue,
};
use sixm_core::{
    EventContext, EventSource, Host, ResourceName, ScriptError, ScriptRuntime, Value, WebRequest,
};

use crate::events::Events;
use crate::exports::Exports;
use crate::kvp::Finds;
use crate::natives;
use crate::refs::RefTable;
use crate::sandbox;
use crate::scheduler::Sched;
use crate::state::ChangeHandlers;

/// CPU budget for executing a script file during `load_script`.
pub(crate) const SCRIPT_LOAD_BUDGET: Duration = Duration::from_millis(500);
/// CPU budget for ticks, timers, events, exports, and callbacks.
pub(crate) const CALLBACK_BUDGET: Duration = Duration::from_millis(100);

/// The side of the connection a runtime runs on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Side {
    Server,
    /// `local_player` is the client's server id, fixed for the whole session.
    Client {
        local_player: u32,
    },
}

/// RAII guard for re-entrant execution CPU budgets.
///
/// The outermost entry sets `deadline`; nested entries leave it untouched so
/// re-entrant calls cannot extend or reset the outer CPU budget.
pub(crate) struct BudgetGuard {
    deadline: Rc<Cell<Option<Instant>>>,
    owned: bool,
}

impl BudgetGuard {
    fn enter(deadline: &Rc<Cell<Option<Instant>>>, budget: Duration) -> Self {
        if deadline.get().is_none() {
            deadline.set(Some(Instant::now() + budget));
            Self {
                deadline: Rc::clone(deadline),
                owned: true,
            }
        } else {
            Self {
                deadline: Rc::clone(deadline),
                owned: false,
            }
        }
    }
}

impl Drop for BudgetGuard {
    fn drop(&mut self) {
        if self.owned {
            self.deadline.set(None);
        }
    }
}

#[derive(Default)]
pub(crate) struct Rejections {
    current: Vec<(Persistent<JsValue<'static>>, Persistent<JsValue<'static>>)>,
    to_report: Vec<(Persistent<JsValue<'static>>, Persistent<JsValue<'static>>)>,
}

pub(crate) type QueuedEvent = (String, EventSource, Vec<Value>, Option<ResourceName>);
pub(crate) type QueuedStateChange = (String, String, Value, bool);
pub(crate) type QueuedConvarChange = (String, Option<String>, bool);

/// A sandboxed `QuickJS` state running one resource instance.
///
/// Create it with [`JsRuntime::new`], run the manifest's scripts with
/// [`load_script`](Self::load_script), then drive it through [`ScriptRuntime`].
pub struct JsRuntime {
    pub(crate) resource: ResourceName,
    pub(crate) instance: u64,
    pub(crate) root: PathBuf,
    pub(crate) declared_exports: HashSet<String>,
    pub(crate) host: Weak<dyn Host>,
    pub(crate) side: Side,
    pub(crate) me: Weak<Self>,
    // `ctx` is declared before `rt` so the context drops before the runtime.
    pub(crate) ctx: Context,
    pub(crate) rt: Runtime,
    pub(crate) budget_deadline: Rc<Cell<Option<Instant>>>,
    pub(crate) current_source: Cell<EventSource>,
    pub(crate) current_invoker: RefCell<Option<ResourceName>>,
    pub(crate) shut: Cell<bool>,
    pub(crate) in_ctx: Cell<bool>,
    pub(crate) skip_self_event: Cell<bool>,
    pub(crate) skip_self_state: Cell<bool>,
    pub(crate) pending_events: RefCell<VecDeque<QueuedEvent>>,
    pub(crate) pending_state_changes: RefCell<VecDeque<QueuedStateChange>>,
    pub(crate) pending_convar_changes: RefCell<VecDeque<QueuedConvarChange>>,
    pub(crate) func_ref_symbol: RefCell<Option<Persistent<Symbol<'static>>>>,
    pub(crate) sched: RefCell<Sched>,
    pub(crate) events: RefCell<Events>,
    pub(crate) state: RefCell<ChangeHandlers>,
    pub(crate) convar_listeners: RefCell<natives::ConvarListeners>,
    pub(crate) exports: RefCell<Exports>,
    pub(crate) refs: RefCell<RefTable>,
    pub(crate) deferred_releases: RefCell<Vec<u32>>,
    pub(crate) kvp_finds: RefCell<Finds>,
    pub(crate) rejections: RefCell<Rejections>,
    /// Cumulative count of invoked event handlers for `resmon`.
    pub(crate) handled_events: Cell<u64>,
}

impl JsRuntime {
    /// Creates the runtime for one instance of `resource`, whose files live under `root`.
    ///
    /// `declared_exports` lists the export names the manifest declares for this
    /// side (`server_export` on server, `export` on client).
    pub fn new(
        resource: ResourceName,
        instance: u64,
        root: PathBuf,
        declared_exports: Vec<String>,
        host: Weak<dyn Host>,
    ) -> Result<Rc<Self>, ScriptError> {
        let side = side_of(&host)?;
        let budget_deadline = Rc::new(Cell::new(None));
        let rt = sandbox::new_runtime(Rc::clone(&budget_deadline))?;
        let ctx = sandbox::new_context(&rt)?;
        let sym = ctx
            .with(|ctx| Symbol::new(ctx.clone()).map(|s| Persistent::save(&ctx, s)))
            .map_err(|err| ScriptError::Runtime(err.to_string()))?;

        let runtime = Rc::new_cyclic(|me| Self {
            resource,
            instance,
            root,
            declared_exports: declared_exports.into_iter().collect(),
            host,
            side,
            me: me.clone(),
            ctx,
            rt,
            budget_deadline,
            current_source: Cell::new(EventSource::Local),
            current_invoker: RefCell::default(),
            shut: Cell::new(false),
            in_ctx: Cell::new(false),
            skip_self_event: Cell::new(false),
            skip_self_state: Cell::new(false),
            pending_events: RefCell::default(),
            pending_state_changes: RefCell::default(),
            pending_convar_changes: RefCell::default(),
            func_ref_symbol: RefCell::new(Some(sym)),
            sched: RefCell::default(),
            events: RefCell::default(),
            state: RefCell::default(),
            convar_listeners: RefCell::default(),
            exports: RefCell::default(),
            refs: RefCell::default(),
            deferred_releases: RefCell::default(),
            kvp_finds: RefCell::default(),
            rejections: RefCell::default(),
            handled_events: Cell::new(0),
        });

        runtime.install_rejection_tracker();
        natives::install(&runtime)?;
        Ok(runtime)
    }

    /// Reads and evaluates one script file in classic global script mode.
    pub fn load_script(&self, path: &Path) -> Result<(), ScriptError> {
        self.ensure_running()?;
        let source = std::fs::read_to_string(path).map_err(|err| {
            ScriptError::Runtime(format!("cannot read {}: {err}", path.display()))
        })?;
        let chunk_name = match path.strip_prefix(&self.root) {
            Ok(relative) => format!("@{}/{}", self.resource, relative.display()),
            Err(_) => format!("@{}", path.display()),
        };

        let _budget = self.enter_budget(SCRIPT_LOAD_BUDGET);
        let outcome = self.with_ctx(|ctx| {
            let mut options = EvalOptions::default();
            options.global = true;
            options.strict = false;
            options.filename = Some(chunk_name);
            sandbox::catch_script(
                ctx,
                ctx.eval_with_options::<JsValue<'_>, _>(source, options),
            )?;
            Ok(())
        });
        self.drain_jobs();
        outcome
    }

    /// Runs `f` inside `self.ctx` while tracking active entry so nested
    /// re-entrant `ScriptRuntime` calls never panic on `rquickjs`'s internal
    /// `RefCell`.
    pub(crate) fn with_ctx<R>(&self, f: impl FnOnce(&Ctx<'_>) -> R) -> R {
        let prev = self.in_ctx.replace(true);
        let out = self.ctx.with(|ctx| f(&ctx));
        self.in_ctx.set(prev);
        if !prev {
            self.flush_pending();
        }
        out
    }

    /// Drains any events or state changes queued while this runtime was active.
    fn flush_pending(&self) {
        while !self.shut.get() {
            let next_convar = self.pending_convar_changes.borrow_mut().pop_front();
            if let Some((name, value, replicated)) = next_convar {
                self.dispatch_convar_change(&name, value.as_deref(), replicated);
                continue;
            }
            let next_state = self.pending_state_changes.borrow_mut().pop_front();
            if let Some((bag, key, value, replicated)) = next_state {
                self.dispatch_state_change(&bag, &key, &value, replicated);
                continue;
            }
            let next_event = self.pending_events.borrow_mut().pop_front();
            if let Some((name, source, args, invoker)) = next_event {
                let ctx = invoker.map_or_else(EventContext::new, EventContext::invoked_by);
                self.dispatch_event(&name, source, &args, &ctx);
                continue;
            }
            break;
        }
    }

    /// Triggers QuickJS garbage collection (useful for releasing cyclic handles in tests).
    #[doc(hidden)]
    pub fn run_gc(&self) {
        if !self.shut.get() {
            self.rt.run_gc();
        }
    }

    pub(crate) fn enter_budget(&self, budget: Duration) -> BudgetGuard {
        BudgetGuard::enter(&self.budget_deadline, budget)
    }

    pub(crate) fn budget_expired(&self) -> bool {
        self.budget_deadline
            .get()
            .is_some_and(|limit| Instant::now() >= limit)
    }

    pub(crate) fn ensure_running(&self) -> Result<(), ScriptError> {
        if self.shut.get() {
            Err(ScriptError::ResourceNotRunning(self.resource.to_string()))
        } else {
            Ok(())
        }
    }

    pub(crate) fn func_ref_symbol(&self) -> Option<Persistent<Symbol<'static>>> {
        self.func_ref_symbol.borrow().clone()
    }

    /// Upgrades a weak handle inside a JS callback, throwing a JS exception on stopped runtimes.
    pub(crate) fn upgrade(ctx: &Ctx<'_>, weak: &Weak<Self>) -> rquickjs::Result<Rc<Self>> {
        weak.upgrade()
            .filter(|rt| !rt.shut.get())
            .ok_or_else(|| Exception::throw_message(ctx, "resource is not running"))
    }

    /// Returns the host, throwing a JS exception if the host has been dropped.
    pub(crate) fn host(&self, ctx: &Ctx<'_>) -> rquickjs::Result<Rc<dyn Host>> {
        self.host
            .upgrade()
            .ok_or_else(|| Exception::throw_message(ctx, "the server is no longer available"))
    }

    /// Returns the host, or a [`ScriptError::Runtime`] if the host has been dropped.
    pub(crate) fn host_or_err(&self) -> Result<Rc<dyn Host>, ScriptError> {
        self.host
            .upgrade()
            .ok_or_else(|| ScriptError::Runtime("the server is no longer available".to_owned()))
    }

    /// Drains `QuickJS` pending microtasks under the active CPU budget.
    pub(crate) fn drain_jobs(&self) {
        if self.in_ctx.get() {
            return;
        }
        let _budget = self.enter_budget(CALLBACK_BUDGET);
        while self.rt.is_job_pending() {
            match self.rt.execute_pending_job() {
                Ok(true) => {}
                Ok(false) => break,
                Err(job_exc) => {
                    let (message, interrupted) = job_exc.0.with(|ctx| {
                        let exc = ctx.catch();
                        let interrupted = exc.is_uncatchable_error() || self.budget_expired();
                        (sandbox::format_caught_value(&ctx, &exc), interrupted)
                    });
                    tracing::error!(target: "sixm::script", "[{}] {message}", self.resource);
                    if interrupted {
                        break;
                    }
                }
            }
        }
    }

    fn install_rejection_tracker(&self) {
        let weak = self.me.clone();
        self.rt.set_host_promise_rejection_tracker(Some(Box::new(
            move |ctx, promise, reason, is_handled| {
                let Some(rt) = weak.upgrade().filter(|r| !r.shut.get()) else {
                    return;
                };
                let p = Persistent::save(&ctx, promise);
                if is_handled {
                    let mut dropped = Vec::new();
                    {
                        let mut rejections = rt.rejections.borrow_mut();
                        drain_matching(&mut rejections.current, &p, &mut dropped);
                        drain_matching(&mut rejections.to_report, &p, &mut dropped);
                    }
                    drop(dropped);
                } else {
                    let r = Persistent::save(&ctx, reason);
                    let mut rejections = rt.rejections.borrow_mut();
                    let already = rejections.current.iter().any(|(known, _)| *known == p)
                        || rejections.to_report.iter().any(|(known, _)| *known == p);
                    if !already {
                        rejections.current.push((p, r));
                    }
                }
            },
        )));
    }

    pub(crate) fn advance_rejections(&self) {
        let mut rejections = self.rejections.borrow_mut();
        let current = std::mem::take(&mut rejections.current);
        rejections.to_report.extend(current);
    }

    pub(crate) fn report_unhandled_rejections(&self) {
        let unhandled = std::mem::take(&mut self.rejections.borrow_mut().to_report);
        if unhandled.is_empty() {
            return;
        }
        self.with_ctx(|ctx| {
            for (_, reason) in &unhandled {
                let msg = match reason.clone().restore(ctx) {
                    Ok(val) => sandbox::format_caught_value(ctx, &val),
                    Err(err) => err.to_string(),
                };
                tracing::warn!(
                    target: "sixm::script",
                    "[{}] unhandled promise rejection: {msg}",
                    self.resource
                );
            }
        });
        drop(unhandled);
    }

    /// Ordered teardown so `QuickJS` `JS_FreeRuntime` never trips GC assertions.
    fn clear_state(&self) {
        let sched = std::mem::take(&mut *self.sched.borrow_mut());
        let events = std::mem::take(&mut *self.events.borrow_mut());
        let state = std::mem::take(&mut *self.state.borrow_mut());
        let convar_listeners = std::mem::take(&mut *self.convar_listeners.borrow_mut());
        let exports = std::mem::take(&mut *self.exports.borrow_mut());
        let refs = std::mem::take(&mut *self.refs.borrow_mut());
        let deferred = std::mem::take(&mut *self.deferred_releases.borrow_mut());
        let kvp_finds = std::mem::take(&mut *self.kvp_finds.borrow_mut());
        let rejections = std::mem::take(&mut *self.rejections.borrow_mut());
        let sym = self.func_ref_symbol.borrow_mut().take();
        drop((
            sched,
            events,
            state,
            convar_listeners,
            exports,
            refs,
            deferred,
            kvp_finds,
            rejections,
            sym,
        ));

        self.ctx.with(|ctx| {
            let globals = ctx.globals();
            let keys: Vec<Atom<'_>> = globals
                .own_keys(Filter::new().string().symbol())
                .filter_map(Result::ok)
                .collect();
            for key in keys {
                let _ = globals.remove(key);
            }
        });

        self.rt.run_gc();
        self.rt.set_interrupt_handler(None);
        self.rt.set_host_promise_rejection_tracker(None);
    }
}

impl Drop for JsRuntime {
    fn drop(&mut self) {
        self.shutdown();
    }
}

impl fmt::Debug for JsRuntime {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("JsRuntime")
            .field("resource", &self.resource)
            .field("instance", &self.instance)
            .field("shut", &self.shut.get())
            .finish_non_exhaustive()
    }
}

impl ScriptRuntime for JsRuntime {
    fn resource(&self) -> &ResourceName {
        &self.resource
    }

    fn instance(&self) -> u64 {
        self.instance
    }

    fn tick(&self, now: Instant) {
        if !self.shut.get() {
            self.run_frame(now);
        }
    }

    fn run_timers(&self, now: Instant) {
        if !self.shut.get() {
            self.run_due_timers(now);
        }
    }

    fn next_deadline(&self) -> Option<Instant> {
        if self.shut.get() {
            None
        } else {
            self.earliest_timer()
        }
    }

    fn handles_event(&self, event: &str) -> bool {
        !self.shut.get() && self.events.borrow().handles(event)
    }

    fn deliver_event(
        &self,
        event: &str,
        source: EventSource,
        args: &[Value],
        ctx: &EventContext,
    ) -> Result<(), ScriptError> {
        self.ensure_running()?;
        self.dispatch_event(event, source, args, ctx);
        Ok(())
    }

    fn call_export(
        &self,
        export: &str,
        args: &[Value],
        invoker: Option<&ResourceName>,
    ) -> Result<Vec<Value>, ScriptError> {
        self.ensure_running()?;
        self.invoke_export(export, args, invoker)
    }

    fn call_ref(&self, id: u32, args: &[Value]) -> Result<Vec<Value>, ScriptError> {
        self.ensure_running()?;
        self.invoke_ref(id, args)
    }

    fn call_web_handler(
        &self,
        id: u32,
        request: u64,
        data: &WebRequest,
    ) -> Result<(), ScriptError> {
        self.ensure_running()?;
        self.invoke_web(id, request, data)
    }

    fn retain_ref(&self, id: u32) -> bool {
        !self.shut.get() && self.retain_function(id)
    }

    fn release_ref(&self, id: u32) {
        if !self.shut.get() {
            self.release_function(id);
        }
    }

    fn deliver_state_change(&self, bag: &str, key: &str, value: &Value, replicated: bool) {
        if !self.shut.get() {
            self.dispatch_state_change(bag, key, value, replicated);
        }
    }

    fn deliver_convar_change(&self, name: &str, value: Option<&str>, replicated: bool) {
        if !self.shut.get() {
            self.dispatch_convar_change(name, value, replicated);
        }
    }

    fn shutdown(&self) {
        if !self.shut.replace(true) {
            self.clear_state();
        }
    }

    fn memory_bytes(&self) -> usize {
        usize::try_from(self.rt.memory_usage().malloc_size.max(0)).unwrap_or(0)
    }

    fn handled_events(&self) -> u64 {
        self.handled_events.get()
    }
}

fn drain_matching(
    list: &mut Vec<(Persistent<JsValue<'static>>, Persistent<JsValue<'static>>)>,
    target: &Persistent<JsValue<'static>>,
    out: &mut Vec<(Persistent<JsValue<'static>>, Persistent<JsValue<'static>>)>,
) {
    let mut i = 0;
    while i < list.len() {
        if list[i].0 == *target {
            out.push(list.swap_remove(i));
        } else {
            i += 1;
        }
    }
}

fn side_of(host: &Weak<dyn Host>) -> Result<Side, ScriptError> {
    let host = host
        .upgrade()
        .ok_or_else(|| ScriptError::Runtime("the server is no longer available".to_owned()))?;
    if host.server().is_some() {
        return Ok(Side::Server);
    }
    match host.client() {
        Some(client) => Ok(Side::Client {
            local_player: client.local_player(),
        }),
        None => Err(ScriptError::Runtime(
            "the host is neither a server nor a client".to_owned(),
        )),
    }
}

/// Converts a `QuickJS` error into [`ScriptError::Runtime`], extracting any pending JS exception.
pub(crate) fn js_err(ctx: &Ctx<'_>, err: rquickjs::Error) -> ScriptError {
    let caught = rquickjs::CaughtError::from_error(ctx, err);
    ScriptError::Runtime(sandbox::format_caught(ctx, caught))
}
