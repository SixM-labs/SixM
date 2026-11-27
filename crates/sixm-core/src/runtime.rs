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
    /// Sent by a connected player (the player's server id). Only handlers
    /// registered with `RegisterNetEvent` receive these.
    Net(u32),
    /// Triggered by the server about a player, such as `playerJoining` or
    /// `playerDropped`. `source` is the player's id, and the event reaches
    /// every handler, because the server itself is trusted.
    Player(u32),
    /// Sent by the server to this client. Only handlers registered with
    /// `RegisterNetEvent` receive it, the same rule as [`Net`](Self::Net), and
    /// scripts see `source == 65535`, as they do in FiveM.
    Server,
}

/// Receiver of a client event. Scripts pass `-1` for [`ClientTarget::All`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClientTarget {
    Player(u32),
    All,
}

/// What the server knows about a connected player.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlayerInfo {
    pub name: String,
    /// Remote address, e.g. `203.0.113.7:51234`.
    pub endpoint: String,
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

/// Services a host provides to script runtimes, on every side.
///
/// A host is either a server or a client, and [`server`](Self::server) and
/// [`client`](Self::client) say which: exactly one of them returns `Some`.
/// Runtimes hold this as a `Weak<dyn Host>`; the host owns the runtimes.
pub trait Host {
    /// Server-only services, when this host is a server.
    fn server(&self) -> Option<&dyn ServerHost>;

    /// Client-only services, when this host is a client.
    fn client(&self) -> Option<&dyn ClientHost>;

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

    /// Contents of the file `path` of resource `resource`, or `None` when the
    /// resource is unknown or the file cannot be read. `path` is normalized
    /// (see [`normalize_resource_path`](crate::normalize_resource_path)).
    fn load_resource_file(&self, resource: &str, path: &str) -> Option<Vec<u8>>;

    /// Writes `data` to the file `path` of resource `resource`, creating its
    /// folders, and returns whether it was written. `path` is normalized.
    fn save_resource_file(&self, resource: &str, path: &str, data: &[u8]) -> bool;

    /// Value stored under `key` in state bag `bag`, or `None` when the bag or
    /// the key is not set.
    fn state_get(&self, bag: &str, key: &str) -> Option<Value>;

    /// Stores `value` under `key` in state bag `bag`, or deletes the key when
    /// `value` is nil. Every write runs the change handlers. A replicated write
    /// is also sent to every client. Fails for an unknown bag, a key that is
    /// empty or too long, or a value that holds a function reference.
    fn state_set(
        &self,
        bag: &str,
        key: &str,
        value: Value,
        replicated: bool,
    ) -> Result<(), ScriptError>;
}

/// Services only a server provides: the connected players and the messages
/// that reach them.
pub trait ServerHost {
    /// Sends an event to one connected player or to all of them. Fails when a
    /// value cannot cross the network (function references).
    fn trigger_client_event(
        &self,
        event: &str,
        target: ClientTarget,
        args: &[Value],
    ) -> Result<(), ScriptError>;

    /// Server ids of the connected players, in join order.
    fn players(&self) -> Vec<u32>;

    /// Name and network endpoint of a connected player.
    fn player_info(&self, player: u32) -> Option<PlayerInfo>;

    /// Disconnects a player with a reason shown to them. Takes effect after
    /// the current script call returns.
    fn drop_player(&self, player: u32, reason: &str);
}

/// Services only a client provides: the connection to the server.
pub trait ClientHost {
    /// Sends an event to the server. Fails for function references.
    fn trigger_server_event(&self, event: &str, args: &[Value]) -> Result<(), ScriptError>;

    /// This client's server id.
    fn local_player(&self) -> u32;
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
    /// resumed by later [`tick`](Self::tick) calls. Network sources (`Net` and
    /// `Server`) reach only handlers registered with `RegisterNetEvent`.
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

    /// Runs this runtime's state change handlers that match `bag` and `key`.
    /// Each handler runs as its own task, like an event handler.
    fn deliver_state_change(&self, bag: &str, key: &str, value: &Value, replicated: bool);

    /// Drops all tasks, handlers and references. The runtime is unusable
    /// afterwards; the host removes it.
    fn shutdown(&self);
}
