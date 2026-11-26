use std::cell::Cell;
use std::path::PathBuf;
use std::time::Instant;

use crate::value::{FuncRef, ResourceName, Value};

/// Where an event came from. Exposed to scripts as `source`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EventSource {
    /// Triggered on this server (script, console or the server itself).
    /// FiveM scripts see `source == ""` for these.
    Local,
    /// Triggered by a connected player (the player's server id).
    Net(u32),
}

/// Shared state of one event dispatch, visible to every handler it reaches.
///
/// `CancelEvent()` in any handler sets the flag; later handlers (also in other
/// resources) observe it through `WasEventCanceled()`.
#[derive(Debug, Default)]
pub struct EventContext {
    canceled: Cell<bool>,
}

impl EventContext {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn cancel(&self) {
        self.canceled.set(true);
    }

    pub fn is_canceled(&self) -> bool {
        self.canceled.get()
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ScriptError {
    #[error("resource `{0}` is not running")]
    ResourceNotRunning(String),
    #[error("resource `{resource}` has no export `{export}`")]
    NoSuchExport { resource: String, export: String },
    #[error("function reference {0:?} is no longer valid")]
    StaleRef(FuncRef),
    #[error("value nested deeper than {0} levels (cyclic table?)")]
    TooDeep(usize),
    #[error("{0}")]
    Runtime(String),
}

/// Services the server provides to script runtimes.
///
/// Runtimes hold this as a `Weak<dyn Host>`; the host owns the runtimes.
pub trait Host {
    /// Dispatches a local event to every running resource that handles it,
    /// in resource start order. Returns whether a handler canceled it.
    fn trigger_event(
        &self,
        event: &str,
        source: EventSource,
        args: &[Value],
    ) -> Result<bool, ScriptError>;

    /// Calls `export` on running resource `resource` synchronously.
    fn call_export(
        &self,
        resource: &str,
        export: &str,
        args: &[Value],
    ) -> Result<Vec<Value>, ScriptError>;

    /// Calls a function reference in whichever runtime owns it.
    fn call_ref(&self, func: &FuncRef, args: &[Value]) -> Result<Vec<Value>, ScriptError>;

    /// Keeps a function alive beyond the call that delivered it. Returns
    /// `false` when the reference is already stale.
    fn retain_ref(&self, func: &FuncRef) -> bool;

    /// Gives back one [`retain_ref`](Self::retain_ref).
    fn release_ref(&self, func: &FuncRef);

    /// Queues a console command. It runs after the current script call
    /// returns, never re-entrantly, so a script can safely stop itself.
    fn execute_command(&self, line: &str);

    /// Registers a console command handled by a script function. A command
    /// registered by a resource is removed when that resource stops.
    fn register_command(&self, name: &str, handler: FuncRef, restricted: bool);

    fn get_convar(&self, name: &str) -> Option<String>;

    /// Root folder of a known resource (running or not).
    fn resource_path(&self, resource: &str) -> Option<PathBuf>;
}

/// One running resource inside one scripting language.
///
/// All methods are re-entrant: a call may arrive while the same runtime is
/// already executing script code further up the stack.
pub trait ScriptRuntime {
    fn resource(&self) -> &ResourceName;

    /// Unique per start of the resource; see [`FuncRef::instance`].
    fn instance(&self) -> u64;

    /// Resumes every scheduled task that is due at `now`, including tasks
    /// that asked to wait for the next frame (`Wait(0)`).
    fn tick(&self, now: Instant);

    /// Resumes only the timer tasks due at `now`. The server calls this when
    /// a timer comes due between frames, so next-frame tasks still run once
    /// per frame.
    fn run_timers(&self, now: Instant);

    /// Earliest timer deadline, ignoring next-frame tasks. The server sleeps
    /// until the earlier of this and its next frame.
    fn next_deadline(&self) -> Option<Instant>;

    /// Cheap check used to skip runtimes during event fan-out.
    fn handles_event(&self, event: &str) -> bool;

    /// Runs this runtime's handlers for `event`. Each handler runs as its own
    /// task: it executes until it finishes or first yields (`Wait`), and is
    /// resumed by later [`tick`](Self::tick) calls.
    fn deliver_event(
        &self,
        event: &str,
        source: EventSource,
        args: &[Value],
        ctx: &EventContext,
    ) -> Result<(), ScriptError>;

    fn call_export(&self, export: &str, args: &[Value]) -> Result<Vec<Value>, ScriptError>;

    fn call_ref(&self, id: u32, args: &[Value]) -> Result<Vec<Value>, ScriptError>;

    /// See [`Host::retain_ref`]. Returns `false` for an unknown id.
    fn retain_ref(&self, id: u32) -> bool;

    /// See [`Host::release_ref`].
    fn release_ref(&self, id: u32);

    /// Drops all tasks, handlers and references. The runtime is unusable
    /// afterwards; the host removes it.
    fn shutdown(&self);
}
