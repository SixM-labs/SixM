//! The [`LuaRuntime`] type: one sandboxed Lua state per resource instance.

use std::cell::{Cell, RefCell};
use std::collections::HashSet;
use std::fmt;
use std::path::{Path, PathBuf};
use std::rc::{Rc, Weak};
use std::time::Instant;

use mlua::chunk::ChunkMode;
use mlua::{FromLuaMulti, Function, IntoLuaMulti, Lua, MultiValue, Table, Value as LuaValue};
use sixm_core::{
    EventContext, EventSource, Host, ResourceName, ScriptError, ScriptRuntime, Value, WebRequest,
};

use crate::events::Events;
use crate::exports::Exports;
use crate::kvp::Finds;
use crate::natives;
use crate::preprocess::lua_backticks;
use crate::promise::Promises;
use crate::refs::RefTable;
use crate::sandbox;
use crate::scheduler::Sched;
use crate::state::ChangeHandlers;

/// The side of the connection a runtime runs on, read from its host once when
/// the runtime is created.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Side {
    Server,
    /// `local_player` is the client's server id, fixed for the whole session.
    Client {
        local_player: u32,
    },
}

/// A sandboxed Lua 5.4 state running one resource instance.
///
/// Create it with [`LuaRuntime::new`], run the manifest's scripts with
/// [`load_script`](Self::load_script), then drive it through [`ScriptRuntime`].
pub struct LuaRuntime {
    pub(crate) resource: ResourceName,
    pub(crate) instance: u64,
    pub(crate) root: PathBuf,
    pub(crate) declared_exports: HashSet<String>,
    pub(crate) host: Weak<dyn Host>,
    /// Which globals this runtime installs, and which side it is.
    pub(crate) side: Side,
    /// Weak handle to this runtime, captured by Lua closures and function handles.
    pub(crate) me: Weak<Self>,
    pub(crate) lua: Lua,
    pub(crate) budget: sandbox::Budget,
    pub(crate) task_wrapper: Function,
    /// `""`, the `source` of local events, kept to avoid a string per resume.
    pub(crate) empty_string: mlua::LuaString,
    /// Source of the task being resumed, inherited by the tasks it creates.
    pub(crate) current_source: Cell<EventSource>,
    /// `(slot, sequence)` of the task being resumed, the one `Await` parks.
    pub(crate) current_task: Cell<Option<(usize, u64)>>,
    /// Invoker of the task being resumed, `None` for host-fired work. Tasks
    /// created while it runs inherit it, and synchronous export calls set it.
    pub(crate) current_invoker: RefCell<Option<ResourceName>>,
    pub(crate) shut: Cell<bool>,
    pub(crate) sched: RefCell<Sched>,
    pub(crate) promises: RefCell<Promises>,
    pub(crate) events: RefCell<Events>,
    pub(crate) state: RefCell<ChangeHandlers>,
    pub(crate) convar_listeners: RefCell<natives::ConvarListeners>,
    pub(crate) exports: RefCell<Exports>,
    pub(crate) refs: RefCell<RefTable>,
    /// Releases requested while `refs` was borrowed, applied at the next safe point.
    pub(crate) deferred_releases: RefCell<Vec<u32>>,
    /// Open `StartFindKvp` cursors.
    pub(crate) kvp_finds: RefCell<Finds>,
    /// Cumulative count of invoked event handlers for `resmon`.
    pub(crate) handled_events: Cell<u64>,
}

impl LuaRuntime {
    /// Creates the runtime for one instance of `resource`, whose files live under `root`.
    ///
    /// `declared_exports` lists the export names the manifest declares for this
    /// side: `server_export` on the server, `export` on a client. A global
    /// function with one of those names is callable as an export.
    pub fn new(
        resource: ResourceName,
        instance: u64,
        root: PathBuf,
        declared_exports: Vec<String>,
        host: Weak<dyn Host>,
    ) -> Result<Rc<Self>, ScriptError> {
        let side = side_of(&host)?;
        let budget = sandbox::Budget::default();
        let lua = sandbox::new_state(&budget).into_script()?;
        let helpers = sandbox::prepare(&lua, budget.clone()).into_script()?;
        let empty_string = lua.create_string("").into_script()?;
        let runtime = Rc::new_cyclic(|me| Self {
            resource,
            instance,
            root,
            declared_exports: declared_exports.into_iter().collect(),
            host,
            side,
            me: me.clone(),
            lua,
            budget,
            task_wrapper: helpers.task_wrapper.clone(),
            empty_string,
            current_source: Cell::new(EventSource::Local),
            current_task: Cell::new(None),
            current_invoker: RefCell::default(),
            shut: Cell::new(false),
            sched: RefCell::default(),
            promises: RefCell::default(),
            events: RefCell::default(),
            state: RefCell::default(),
            convar_listeners: RefCell::default(),
            exports: RefCell::default(),
            refs: RefCell::default(),
            deferred_releases: RefCell::default(),
            kvp_finds: RefCell::default(),
            handled_events: Cell::new(0),
        });
        natives::install(&runtime, &helpers).into_script()?;
        Ok(runtime)
    }

    /// Reads and runs one script file. Its main chunk runs as a task, so it can
    /// call `Wait` at top level.
    pub fn load_script(&self, path: &Path) -> Result<(), ScriptError> {
        self.ensure_running()?;
        let source = std::fs::read(path).map_err(|err| {
            ScriptError::Runtime(format!("cannot read {}: {err}", path.display()))
        })?;
        let source = lua_backticks(&source);
        let chunk_name = match path.strip_prefix(&self.root) {
            Ok(relative) => format!("@{}/{}", self.resource, relative.display()),
            Err(_) => format!("@{}", path.display()),
        };
        let main = self
            .lua
            .load(source.as_ref())
            .set_name(chunk_name)
            .set_mode(ChunkMode::Text)
            .into_function()
            .into_script()?;
        let (slot, _) = self.spawn(main, EventSource::Local, None).into_script()?;
        self.run(slot, Instant::now(), MultiValue::new());
        Ok(())
    }

    /// Fails with [`ScriptError::ResourceNotRunning`] after shutdown.
    pub(crate) fn ensure_running(&self) -> Result<(), ScriptError> {
        if self.shut.get() {
            Err(ScriptError::ResourceNotRunning(self.resource.to_string()))
        } else {
            Ok(())
        }
    }

    /// The host, or a Lua error when the server no longer exists.
    pub(crate) fn host(&self) -> mlua::Result<Rc<dyn Host>> {
        self.host
            .upgrade()
            .ok_or_else(|| mlua::Error::runtime("the server is no longer available"))
    }

    /// Upgrades a weak handle inside a Lua callback, refusing stopped runtimes.
    pub(crate) fn upgrade(weak: &Weak<Self>) -> mlua::Result<Rc<Self>> {
        weak.upgrade()
            .filter(|runtime| !runtime.shut.get())
            .ok_or_else(|| mlua::Error::runtime("resource is not running"))
    }

    /// Drops every task, handler, export and reference this runtime holds.
    ///
    /// Each collection is taken out of its cell before it is dropped, so a
    /// finalizer that releases a reference never meets a live borrow.
    fn clear_state(&self) {
        let sched = std::mem::take(&mut *self.sched.borrow_mut());
        let promises = std::mem::take(&mut *self.promises.borrow_mut());
        let events = std::mem::take(&mut *self.events.borrow_mut());
        let state = std::mem::take(&mut *self.state.borrow_mut());
        let convar_listeners = std::mem::take(&mut *self.convar_listeners.borrow_mut());
        let exports = std::mem::take(&mut *self.exports.borrow_mut());
        let refs = std::mem::take(&mut *self.refs.borrow_mut());
        let deferred = std::mem::take(&mut *self.deferred_releases.borrow_mut());
        drop((
            sched,
            promises,
            events,
            state,
            convar_listeners,
            exports,
            refs,
            deferred,
        ));

        if let Err(err) = self.drop_globals() {
            tracing::warn!(target: "sixm::script", "[{}] shutdown: {err}", self.resource);
        }
    }

    /// Clears the globals and collects garbage, so function handles release
    /// their references while the host still exists.
    fn drop_globals(&self) -> mlua::Result<()> {
        let globals: Table = self.lua.globals();
        let keys = globals
            .pairs::<LuaValue, LuaValue>()
            .map(|pair| pair.map(|(key, _)| key))
            .collect::<mlua::Result<Vec<_>>>()?;
        for key in keys {
            globals.raw_set(key, LuaValue::Nil)?;
        }
        self.lua.gc_collect()
    }
}

impl fmt::Debug for LuaRuntime {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("LuaRuntime")
            .field("resource", &self.resource)
            .field("instance", &self.instance)
            .field("shut", &self.shut.get())
            .finish_non_exhaustive()
    }
}

impl ScriptRuntime for LuaRuntime {
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
        self.dispatch_event(event, source, args, ctx)
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

    fn shutdown(&self) {
        if !self.shut.replace(true) {
            self.clear_state();
        }
    }

    fn memory_bytes(&self) -> usize {
        self.lua.used_memory()
    }

    fn handled_events(&self) -> u64 {
        self.handled_events.get()
    }
}

/// Asks the host which side it is. A host that is both is treated as a server.
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

/// Converts a Lua result into the contract's error type.
pub(crate) trait IntoScriptResult<T> {
    /// Maps a Lua error to [`ScriptError::Runtime`] with its message.
    fn into_script(self) -> Result<T, ScriptError>;
}

impl<T> IntoScriptResult<T> for mlua::Result<T> {
    fn into_script(self) -> Result<T, ScriptError> {
        self.map_err(|err| ScriptError::Runtime(err.to_string()))
    }
}

/// Maps a contract error into a Lua error that can be raised from a callback.
pub(crate) fn lua_err(err: ScriptError) -> mlua::Error {
    mlua::Error::runtime(err)
}

/// Creates a Lua function that upgrades `weak` and then calls `f` with the runtime.
pub(crate) fn bind_fn<A, R, F>(lua: &Lua, weak: &Weak<LuaRuntime>, f: F) -> mlua::Result<Function>
where
    A: FromLuaMulti,
    R: IntoLuaMulti,
    F: Fn(&Rc<LuaRuntime>, A) -> mlua::Result<R> + 'static,
{
    let weak = weak.clone();
    lua.create_function(move |_, args: A| {
        let runtime = LuaRuntime::upgrade(&weak)?;
        f(&runtime, args)
    })
}
