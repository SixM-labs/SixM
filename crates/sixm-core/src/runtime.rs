use std::cell::Cell;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use crate::entity::{EntityInfo, EntityKind};
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

/// Lifecycle state of a resource, as `GetResourceState` names it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResourceState {
    /// The host does not know the resource.
    Missing,
    /// Known, and not running.
    Stopped,
    /// Its scripts are loading. `onResourceStart` has not run yet.
    Starting,
    /// Running.
    Started,
    /// Its stop events are running, and it is about to stop.
    Stopping,
}

impl ResourceState {
    /// The name `GetResourceState` returns: `missing`, `stopped`, `starting`,
    /// `started` or `stopping`.
    pub fn name(self) -> &'static str {
        match self {
            Self::Missing => "missing",
            Self::Stopped => "stopped",
            Self::Starting => "starting",
            Self::Started => "started",
            Self::Stopping => "stopping",
        }
    }
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
    invoker: Option<ResourceName>,
}

impl EventContext {
    /// A dispatch that no resource started: the console, the network, or the
    /// host itself. `GetInvokingResource()` is nil in its handlers.
    pub fn new() -> Self {
        Self::default()
    }

    /// A dispatch started by a script of resource `invoker`, which handlers see
    /// through `GetInvokingResource()`.
    pub fn invoked_by(invoker: ResourceName) -> Self {
        Self {
            canceled: Cell::new(false),
            invoker: Some(invoker),
        }
    }

    /// The resource whose script started this dispatch, if one did.
    pub fn invoker(&self) -> Option<&ResourceName> {
        self.invoker.as_ref()
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

/// An outgoing HTTP request, as a script starts it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HttpRequest {
    /// The absolute `http` or `https` URL.
    pub url: String,
    /// The request method, such as `GET`. Validated by the host.
    pub method: String,
    /// Request headers, in the order the script gave them.
    pub headers: Vec<(String, String)>,
    /// The request body, when the script sent `data`.
    pub body: Option<Vec<u8>>,
    /// Overrides the host's default timeout, from `options.timeout`.
    pub timeout: Option<Duration>,
}

/// An incoming HTTP request on the server's TCP listener, as a resource handler
/// sees it. The body is read in full before the handler runs, and it is limited
/// to 1 MiB.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WebRequest {
    /// The request method, such as `GET`.
    pub method: String,
    /// The path of the request, without the query, such as `/res/page`.
    pub path: String,
    /// The query string, without the `?`. Empty when there is none.
    pub query: String,
    /// Request headers, in the order they were sent. Names are lower case.
    pub headers: Vec<(String, String)>,
    /// The request body.
    pub body: Vec<u8>,
}

/// What a resource sends back for a request, once its response is complete.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WebResponse {
    /// The status code, such as `200`.
    pub status: u16,
    /// Response headers. Names and values are validated by the listener.
    pub headers: Vec<(String, String)>,
    /// The response body.
    pub body: Vec<u8>,
}

/// One step of a resource's answer to a request: its head, a piece of the
/// body, or the last piece, which completes the response.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WebReply {
    /// Sets the status and headers. Replaces any earlier head.
    Head {
        status: u16,
        headers: Vec<(String, String)>,
    },
    /// Appends to the body; the response is not complete yet.
    Write(Vec<u8>),
    /// Appends the last piece of the body and completes the response.
    End(Vec<u8>),
}

/// How the server treats a convar's value, decided by the command or function
/// that stored it last.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConvarKind {
    /// Kept on the server. The default, set by `set` and `SetConvar`.
    Plain,
    /// Marked as server information, set by `sets` and `SetConvarServerInfo`.
    /// Nothing sends it anywhere yet.
    ServerInfo,
    /// Sent to every connected client, and to each player who joins. Set by
    /// `setr` and `SetConvarReplicated`.
    Replicated,
}

impl ConvarKind {
    /// The kind's name, as the console shows it.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Plain => "plain",
            Self::ServerInfo => "server info",
            Self::Replicated => "replicated",
        }
    }
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
    /// `invoker` is the resource whose script triggered it, which handlers see
    /// as `GetInvokingResource()`.
    fn trigger_event(
        &self,
        event: &str,
        source: EventSource,
        args: &[Value],
        invoker: Option<&ResourceName>,
    ) -> Result<bool, ScriptError>;

    /// Calls `export` on running resource `resource` synchronously. `invoker`
    /// is the resource whose script called it, which the export sees as
    /// `GetInvokingResource()`.
    fn call_export(
        &self,
        resource: &str,
        export: &str,
        args: &[Value],
        invoker: Option<&ResourceName>,
    ) -> Result<Vec<Value>, ScriptError>;

    /// The lifecycle state of `resource`.
    fn resource_state(&self, resource: &str) -> ResourceState;

    /// Names of the resources this host knows: on a server every resource it
    /// has scanned, sorted by name; on a client the running resources, in start
    /// order.
    fn resource_names(&self) -> Vec<ResourceName>;

    /// The values of metadata key `key` in the manifest of `resource`, in
    /// declaration order, such as `["1.2.0"]` for `version '1.2.0'`. Empty when
    /// the resource or the key is unknown. Clients know the metadata of running
    /// resources only.
    fn resource_metadata(&self, resource: &str, key: &str) -> Vec<String>;

    /// Calls a function reference in whichever runtime owns it.
    fn call_ref(&self, func: &FuncRef, args: &[Value]) -> Result<Vec<Value>, ScriptError>;

    /// Keeps a function alive beyond the call that delivered it. Returns
    /// `false` when the reference is already stale.
    fn retain_ref(&self, func: &FuncRef) -> bool;

    /// Gives back one [`retain_ref`](Self::retain_ref).
    fn release_ref(&self, func: &FuncRef);

    /// Starts an outgoing HTTP request for `resource` (`PerformHttpRequest`).
    /// The host calls `callback` once with the outcome, from a later frame,
    /// and releases it afterwards. When the request cannot start, the error is
    /// returned and the callback is released at once.
    fn perform_http_request(
        &self,
        resource: &ResourceName,
        request: HttpRequest,
        callback: FuncRef,
    ) -> Result<(), ScriptError>;

    /// Queues a console command. It runs after the current script call
    /// returns, never re-entrantly, so a script can safely stop itself.
    /// `invoker` is the resource whose script called it: the line runs with
    /// that resource's rights, as its principal `resource.<name>`. `None` runs
    /// it with console rights, which only the host itself uses.
    fn execute_command(&self, line: &str, invoker: Option<&ResourceName>);

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

    /// The value stored under `key` in the key-value store of `resource`, or
    /// `None`. A resource only ever reads its own store.
    fn kvp_get(&self, resource: &str, key: &str) -> Option<Vec<u8>>;

    /// Stores `value` under `key` in the key-value store of `resource`, and
    /// returns whether it was stored. Fails for a key that is not 1 to 256
    /// bytes or a value over 64 KiB. The host writes changes to the disk in
    /// the background, in batches, and never blocks the caller on it.
    fn kvp_set(&self, resource: &str, key: &str, value: &[u8]) -> bool;

    /// Removes `key` from the key-value store of `resource`. Returns whether it
    /// was set.
    fn kvp_delete(&self, resource: &str, key: &str) -> bool;

    /// The keys of the key-value store of `resource` that start with `prefix`,
    /// in byte order.
    fn kvp_keys(&self, resource: &str, prefix: &str) -> Vec<String>;

    /// Hands every pending key-value change, of every resource, to the
    /// background writer now instead of with the next batch. Does not wait for
    /// the disk.
    fn kvp_flush(&self);

    /// The entity with id `entity`, or `None` when it does not exist. On the
    /// server this is the entity itself; on a client it is the last state the
    /// server sent.
    fn entity(&self, entity: u32) -> Option<EntityInfo>;

    /// Ids of every existing entity of `kind`, or of every kind when `kind` is
    /// `None`, in ascending order.
    fn entities(&self, kind: Option<EntityKind>) -> Vec<u32>;

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

    /// Sends a paced event to one player or every player.
    fn trigger_latent_client_event(
        &self,
        event: &str,
        target: ClientTarget,
        args: &[Value],
        bps: u64,
    ) -> Result<(), ScriptError>;

    /// Registers the HTTP handler of `resource` (`SetHttpHandler`), replacing
    /// the one it had. The host removes it when the resource stops.
    fn set_web_handler(&self, resource: &ResourceName, handler: FuncRef);

    /// Applies one step of the answer to the request `request`. The last step
    /// completes the response. Fails when the request is unknown, which means
    /// it was already answered or timed out.
    fn web_reply(&self, request: u64, reply: WebReply) -> Result<(), ScriptError>;

    /// Server ids of the connected players, in join order.
    fn players(&self) -> Vec<u32>;

    /// Name and network endpoint of a connected player. A player who is
    /// connecting is known from the start of `playerConnecting`.
    fn player_info(&self, player: u32) -> Option<PlayerInfo>;

    /// Round-trip latency for a connected player, if known.
    fn player_ping(&self, _player: u32) -> Option<Duration> {
        None
    }

    /// The identifiers of a connected or connecting player, such as
    /// `license:<hex>` and `ip:<address>`, in a fixed order. `license` comes
    /// from the key the player's client proves it holds, so it stays the same
    /// across connections from the same client. Empty for an unknown player.
    fn player_identifiers(&self, player: u32) -> Vec<String>;

    /// Whether `principal` may use `object` under the server's access control
    /// entries (`add_ace`, `add_principal`).
    ///
    /// Objects are dotted paths: an entry for `command` also covers
    /// `command.stop`. The most specific path that has any entry for the
    /// principal or one of its parents decides, and at that path a deny beats an
    /// allow. Without any entry the answer is no.
    ///
    /// Every principal inherits from `builtin.everyone`. A connected player
    /// `player.<id>` also inherits from `identifier.<identifier>` for each of
    /// its identifiers, so entries can name a player by `license`.
    fn is_ace_allowed(&self, principal: &str, object: &str) -> bool;

    /// Disconnects a player with a reason shown to them. Takes effect after
    /// the current script call returns.
    fn drop_player(&self, player: u32, reason: &str);

    /// Creates an entity and returns its id. Ids count up from 1 and are not
    /// reused while the server runs. The entity's state bag exists from now on,
    /// every client is told at once, and `entityCreated` runs with the id.
    /// Fails for a transform that is not finite, or when
    /// [`MAX_ENTITIES`](crate::MAX_ENTITIES) entities already exist.
    fn create_entity(&self, info: EntityInfo) -> Result<u32, ScriptError>;

    /// Deletes an entity. `entityRemoved` runs first, while it still exists;
    /// then it and its state bag are removed and every client is told.
    /// Returns `false` when it did not exist.
    fn delete_entity(&self, entity: u32) -> bool;

    /// Moves an entity. Clients receive the latest position and rotation once
    /// per server frame, however often it moved. A move by the server also
    /// reaches the owning client, which then takes the new position as its own.
    /// Fails for an unknown entity or a position that is not finite.
    fn set_entity_position(&self, entity: u32, position: [f32; 3]) -> Result<(), ScriptError>;

    /// Rotates an entity, sent to clients like
    /// [`set_entity_position`](Self::set_entity_position). Fails for an unknown
    /// entity or a rotation that is not finite.
    fn set_entity_rotation(&self, entity: u32, rotation: [f32; 3]) -> Result<(), ScriptError>;

    /// Stores a convar with the given kind, as `SetConvar`, `SetConvarReplicated`
    /// and `SetConvarServerInfo` do. The kind of the last store decides whether
    /// clients receive the value. Fails for a replicated convar whose name or
    /// value is too long to send, and then nothing changes.
    fn set_convar(&self, name: &str, value: &str, kind: ConvarKind) -> Result<(), ScriptError>;

    /// The ped of a connected player. It is created, owned by the player, just
    /// before `playerJoining`, and deleted after `playerDropped`.
    fn player_ped(&self, player: u32) -> Option<u32>;

    /// The routing bucket of a connected player, or `None` when it is not
    /// connected. Players start in bucket 0.
    fn player_routing_bucket(&self, player: u32) -> Option<u32>;

    /// Moves a connected player and its ped to `bucket`. A player only holds
    /// the entities of its own bucket. Returns `false` when the player is not
    /// connected.
    fn set_player_routing_bucket(&self, player: u32, bucket: u32) -> bool;

    /// The routing bucket of an entity, or `None` when it does not exist.
    /// Entities start in bucket 0.
    fn entity_routing_bucket(&self, entity: u32) -> Option<u32>;

    /// Moves an entity to `bucket`, and tells the players that gain or lose
    /// it. Returns `false` when the entity does not exist.
    fn set_entity_routing_bucket(&self, entity: u32, bucket: u32) -> bool;

    /// The connected players in `bucket`, in ascending order.
    fn players_in_routing_bucket(&self, bucket: u32) -> Vec<u32>;

    /// Stores whether the population of `bucket` is enabled. Nothing acts on
    /// it until a game adapter exists. Enabled by default.
    fn set_routing_bucket_population_enabled(&self, bucket: u32, enabled: bool);

    /// Whether the population of `bucket` is enabled.
    fn routing_bucket_population_enabled(&self, bucket: u32) -> bool;

    /// Stores the entity lockdown mode of `bucket`: `inactive`, `relaxed` or
    /// `strict`. Fails for any other mode.
    fn set_routing_bucket_entity_lockdown_mode(
        &self,
        bucket: u32,
        mode: &str,
    ) -> Result<(), ScriptError>;

    /// The entity lockdown mode of `bucket`, `inactive` unless set.
    fn routing_bucket_entity_lockdown_mode(&self, bucket: u32) -> String;

    /// Gives the entity `entity` to player `owner`, or to the server when
    /// `owner` is 0. The entity keeps its teleport epoch: the new owner starts
    /// from the current one. Returns `false` for an unknown entity, or for a
    /// player that is not connected.
    fn set_entity_owner(&self, entity: u32, owner: u32) -> bool;

    /// Sets what happens to an entity whose creator leaves: `delete`, the
    /// default, or `migrate`, which hands it to a player that holds it.
    /// Fails for any other mode.
    fn set_entity_orphan_mode(&self, entity: u32, mode: &str) -> Result<(), ScriptError>;
}

/// Services only a client provides: the connection to the server.
pub trait ClientHost {
    /// Sends an event to the server. Fails for function references.
    fn trigger_server_event(&self, event: &str, args: &[Value]) -> Result<(), ScriptError>;

    /// Sends a paced event to the server.
    fn trigger_latent_server_event(
        &self,
        event: &str,
        args: &[Value],
        bps: u64,
    ) -> Result<(), ScriptError>;

    /// This client's server id.
    fn local_player(&self) -> u32;

    /// The local player's ped, once the server has sent it.
    fn local_ped(&self) -> Option<u32>;

    /// Moves or rotates an entity this client owns. The server receives the
    /// latest transform once per client frame and passes it on to the other
    /// clients. Fails for an entity this client does not own, or for a value
    /// that is not finite. `None` keeps that part of the transform.
    fn move_owned_entity(
        &self,
        entity: u32,
        position: Option<[f32; 3]>,
        rotation: Option<[f32; 3]>,
    ) -> Result<(), ScriptError>;

    /// Asks the server for an entity this client will own. The entity exists
    /// at once, under an id the server leased to this client, and is removed
    /// again when the server refuses it. Fails when this client already has
    /// `sixm_maxClientEntities` entities of its own, or when no leased id is
    /// available yet.
    fn create_network_entity(
        &self,
        kind: EntityKind,
        model: u32,
        position: [f32; 3],
        rotation: [f32; 3],
    ) -> Result<u32, ScriptError>;

    /// Asks the server to give this client the entity `entity`. Returns `false`
    /// when the entity is unknown or already owned, and `true` when the request
    /// is sent. The server may still refuse it, and then the ownership does not
    /// change.
    fn request_entity_control(&self, entity: u32) -> Result<bool, ScriptError>;
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

    /// Calls `export` synchronously. `invoker` is what the export sees as
    /// `GetInvokingResource()` while it runs.
    fn call_export(
        &self,
        export: &str,
        args: &[Value],
        invoker: Option<&ResourceName>,
    ) -> Result<Vec<Value>, ScriptError>;

    fn call_ref(&self, id: u32, args: &[Value]) -> Result<Vec<Value>, ScriptError>;

    /// Runs the HTTP handler `id` for the request `request`, which the host
    /// answers through [`ServerHost::web_reply`]. The handler receives the
    /// request and its response object; a body handler it registered gets the
    /// body afterwards.
    fn call_web_handler(&self, id: u32, request: u64, data: &WebRequest)
    -> Result<(), ScriptError>;

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
