//! The resource manager: installed and running resources, script commands, and
//! the [`Host`] that script runtimes call back into.

use std::cell::{Cell, RefCell};
use std::collections::{BTreeMap, VecDeque};
use std::path::PathBuf;
use std::rc::{Rc, Weak};
use std::time::Instant;

use sixm_core::{
    ClientHost, ClientTarget, ConvarKind, EntityInfo, EntityKind, EventContext, EventSource,
    FuncRef, Host, HttpRequest, PlayerInfo, ResourceName, ResourceState, ScriptError,
    ScriptRuntime, ServerHost, Value,
};
use sixm_kvp::KvpStore;
use sixm_lua::LuaRuntime;
use sixm_net::{
    MAX_CONVAR_NAME_LEN, MAX_CONVAR_VALUE_LEN, MAX_EXPORT_NAME_LEN, MAX_EXPORTS, MAX_METADATA,
    MAX_METADATA_KEY_LEN, MAX_METADATA_VALUE_LEN, PublishedResource, Traffic,
};
use sixm_resources::{Catalog, DependencyError, ResourceError, ScanIssue};
use tracing::{debug, error, info, warn};

use crate::access::AccessStore;
use crate::buckets::BucketSettings;
use crate::commands::CommandSource;
use crate::convars::Convars;
use crate::entities::{EntityStore, EntityUpdate};
use crate::host_functions::{HostFunctions, is_host};
use crate::http::{Completion, HttpClient};
use crate::network::Network;
use crate::perf::FrameHistory;
use crate::resource_files::{FileHashes, listing};
use crate::scope::Scopes;
use crate::state::StateStore;

/// Why a resource operation did not complete.
#[derive(Debug, thiserror::Error)]
pub enum ManagerError {
    /// The dependency graph could not be ordered: unknown resource, missing
    /// dependency or a cycle.
    #[error(transparent)]
    Dependency(#[from] DependencyError),
    /// The operation needs a running resource, and this one is not running.
    #[error("resource `{0}` is not running")]
    NotRunning(String),
    /// A handler of `onResourceStarting` canceled the start.
    #[error("start of `{0}` was canceled by onResourceStarting")]
    Canceled(String),
    /// The resource's script list could not be expanded.
    #[error(transparent)]
    Resource(#[from] ResourceError),
    /// The Lua state for the resource could not be created.
    #[error("cannot create the runtime for `{resource}`: {source}")]
    Runtime {
        /// The resource being started.
        resource: String,
        /// Why the state could not be created.
        source: ScriptError,
    },
}

/// A console command registered by a script with `RegisterCommand`.
#[derive(Debug, Clone)]
pub(crate) struct ScriptCommand {
    /// The function to call. The manager holds one retain on it.
    pub(crate) handler: FuncRef,
    /// Whether the command is marked restricted. Shown by `help` only, since
    /// all input today comes from the console.
    pub(crate) restricted: bool,
}

/// Owns the installed resources and the running ones, and implements [`Host`]
/// for the runtimes it creates.
///
/// Everything runs on one thread. No `RefCell` borrow is held while script code
/// runs, so script calls can re-enter the manager.
#[derive(Debug)]
pub struct ResourceManager {
    /// Handed to every runtime as its host.
    me: Weak<Self>,
    catalog: RefCell<Catalog>,
    scanned: Cell<bool>,
    pub(crate) convars: RefCell<Convars>,
    /// Running resources in start order.
    running: RefCell<Vec<Rc<LuaRuntime>>>,
    /// The running runtimes as one shared slice, rebuilt when `running` changes.
    /// Event fan-out and frames clone this `Rc` instead of allocating.
    snapshot: RefCell<Rc<[Rc<LuaRuntime>]>>,
    next_instance: Cell<u64>,
    /// Script commands by lower-case name.
    commands: RefCell<BTreeMap<String, ScriptCommand>>,
    /// Command lines queued by `ExecuteCommand` and the host, run after the
    /// current call, each with the source that queued it.
    queued: RefCell<VecDeque<(String, CommandSource)>>,
    quit: Cell<bool>,
    /// The listening socket, connected players and pending kicks.
    pub(crate) network: Network,
    /// State bags and their keys.
    pub(crate) state: RefCell<StateStore>,
    /// Every entity and the transforms queued for the next frame.
    pub(crate) entities: RefCell<EntityStore>,
    /// The radius in use, and the entities each player holds.
    pub(crate) scopes: RefCell<Scopes>,
    /// Population and entity lockdown settings of each routing bucket.
    pub(crate) bucket_settings: RefCell<BucketSettings>,
    /// Reused between frames to hold the transforms sent to clients.
    pub(crate) entity_updates: RefCell<Vec<EntityUpdate>>,
    /// Access control entries and principal links, changed by the `add_ace` family.
    pub(crate) access: RefCell<AccessStore>,
    /// Functions the server offers to scripts, such as `setKickReason`.
    pub(crate) host_functions: RefCell<HostFunctions>,
    /// Durations of the recent frames, for the `perf` command.
    pub(crate) frames: RefCell<FrameHistory>,
    /// When `perf` last ran, and the traffic totals it saw then. The rates
    /// cover the time since this mark. It starts at the server's start.
    pub(crate) perf_mark: Cell<(Instant, Traffic)>,
    /// Client files of the running resources, in start order. Advertised to
    /// players as a whole whenever it changes.
    advertised: RefCell<Vec<PublishedResource>>,
    /// Hashes of the client files, so that restarts do not read them again.
    hashes: RefCell<FileHashes>,
    /// Lifecycle states that override the running state while a resource
    /// starts or stops, oldest first.
    transitions: RefCell<Vec<(ResourceName, ResourceState)>>,
    /// Outgoing HTTP requests started by the scripts.
    http: HttpClient,
    /// The key-value store of the resources, opened on first use in `sixm_kvpPath`.
    kvp: RefCell<Option<KvpStore>>,
}

impl ResourceManager {
    /// Creates a manager with the default convars. Nothing is scanned until the
    /// first start or refresh.
    pub fn new() -> Rc<Self> {
        Rc::new_cyclic(|me| Self {
            me: me.clone(),
            catalog: RefCell::default(),
            scanned: Cell::new(false),
            convars: RefCell::new(Convars::new()),
            running: RefCell::default(),
            snapshot: RefCell::new(Rc::from(Vec::new())),
            next_instance: Cell::new(1),
            commands: RefCell::default(),
            queued: RefCell::default(),
            quit: Cell::new(false),
            network: Network::default(),
            state: RefCell::default(),
            entities: RefCell::default(),
            scopes: RefCell::default(),
            bucket_settings: RefCell::default(),
            entity_updates: RefCell::default(),
            access: RefCell::default(),
            host_functions: RefCell::default(),
            frames: RefCell::default(),
            perf_mark: Cell::new((Instant::now(), Traffic::default())),
            advertised: RefCell::default(),
            hashes: RefCell::default(),
            transitions: RefCell::default(),
            http: HttpClient::default(),
            kvp: RefCell::default(),
        })
    }

    /// Starts `name`, after any of its dependencies that are not running yet.
    ///
    /// A resource that fails to load its scripts is logged and the rest still
    /// load. A canceled or unloadable dependency stops the chain.
    pub fn start(&self, name: &str) -> Result<(), ManagerError> {
        self.ensure_scanned();
        if self.is_running(name) {
            info!("Resource {name} is already running");
            return Ok(());
        }
        let order = self.catalog.borrow().start_order(name)?;
        for resource in &order {
            if !self.is_running(resource) {
                self.start_one(resource)?;
            }
        }
        Ok(())
    }

    /// Stops `name` after stopping the running resources that depend on it.
    pub fn stop(&self, name: &str) -> Result<(), ManagerError> {
        self.stop_with_dependents(name).map(|_| ())
    }

    /// Stops `name` and its running dependents, then starts them again. The
    /// dependents start in dependency order after `name`. A resource that is
    /// not running is started.
    pub fn restart(&self, name: &str) -> Result<(), ManagerError> {
        if !self.is_running(name) {
            return self.start(name);
        }
        let dependents = self.stop_with_dependents(name)?;
        self.start(name)?;
        for dependent in dependents.iter().rev() {
            self.start(dependent)?;
        }
        Ok(())
    }

    /// Restarts `name` if it is running, and starts it otherwise.
    pub fn ensure(&self, name: &str) -> Result<(), ManagerError> {
        if self.is_running(name) {
            self.restart(name)
        } else {
            self.start(name)
        }
    }

    /// Scans the resource folder again. Running resources are not affected.
    pub fn refresh(&self) {
        let root = self.convars.borrow().resources_path();
        let (catalog, issues) = Catalog::scan(std::slice::from_ref(&root));
        for issue in &issues {
            match issue {
                ScanIssue::Duplicate {
                    name,
                    kept,
                    ignored,
                } => warn!(
                    "resource `{name}` in {} is ignored; {} is used",
                    ignored.display(),
                    kept.display()
                ),
                ScanIssue::Invalid { error, .. } => warn!("skipping resource: {error}"),
            }
        }
        info!("Found {} resources in {}", catalog.len(), root.display());
        *self.catalog.borrow_mut() = catalog;
        self.scanned.set(true);
    }

    /// Whether `name` is running.
    pub fn is_running(&self, name: &str) -> bool {
        self.running
            .borrow()
            .iter()
            .any(|runtime| runtime.resource().as_ref() == name)
    }

    /// Names of the running resources, in start order.
    pub fn running_resources(&self) -> Vec<ResourceName> {
        self.running
            .borrow()
            .iter()
            .map(|runtime| runtime.resource().clone())
            .collect()
    }

    /// Stores a convar with `kind`. Names are case-insensitive, and a name must
    /// not be empty.
    ///
    /// The kind of the last store decides what clients get. A replicated convar
    /// is sent to every connected player at once. A convar that was replicated
    /// and is stored with another kind is removed on their side. A replicated
    /// convar must fit the wire limits, and the call fails without changing
    /// anything when it does not.
    pub fn set_convar(&self, name: &str, value: &str, kind: ConvarKind) -> Result<(), ScriptError> {
        if name.is_empty() {
            return Err(ScriptError::Runtime("a convar needs a name".to_owned()));
        }
        if kind == ConvarKind::Replicated {
            check_replicated(name, value)?;
        }
        let previous = self.convars.borrow_mut().set(name, value, kind);
        let key = name.to_ascii_lowercase();
        match (kind, previous) {
            (ConvarKind::Replicated, _) => self.broadcast_convar(&key, Some(value)),
            (_, Some(ConvarKind::Replicated)) => self.broadcast_convar(&key, None),
            _ => {}
        }
        Ok(())
    }

    /// Tells every accepted player that the replicated convar `name` changed.
    /// A `None` value removes it on their side.
    fn broadcast_convar(&self, name: &str, value: Option<&str>) {
        self.with_server(|server| {
            if let Err(err) = server.broadcast_convars(&[(name, value)]) {
                warn!("cannot send convar `{name}` to the players: {err}");
            }
        });
    }

    /// Sends every replicated convar to `player`, who is being admitted.
    pub(crate) fn join_convars(&self, player: u32) {
        let replicated = self.convars.borrow().replicated();
        let entries: Vec<(&str, Option<&str>)> = replicated
            .iter()
            .map(|(name, value)| (name.as_str(), Some(value.as_str())))
            .collect();
        self.with_server(|server| {
            if let Err(err) = server.send_convars(player, &entries) {
                warn!("cannot send the convars to player {player}: {err}");
            }
        });
    }

    /// The value of a convar, if it is set.
    pub fn convar(&self, name: &str) -> Option<String> {
        self.convars.borrow().get(name).map(str::to_owned)
    }

    /// The kind of a convar, if it is set.
    pub fn convar_kind(&self, name: &str) -> Option<ConvarKind> {
        self.convars.borrow().kind(name)
    }

    /// Asks the server loop to stop after the current command.
    pub fn request_quit(&self) {
        self.quit.set(true);
    }

    /// Whether a quit was requested.
    pub fn quit_requested(&self) -> bool {
        self.quit.get()
    }

    /// Frames per second, from the convar. See [`Convars::tick_rate`].
    pub(crate) fn tick_rate(&self) -> u64 {
        self.convars.borrow().tick_rate()
    }

    /// Stops every running resource, last started first, so each
    /// `onResourceStop` handler runs while its resource is still up.
    pub(crate) fn stop_all(&self) {
        loop {
            let last = self.running.borrow().last().cloned();
            let Some(runtime) = last else {
                return;
            };
            self.stop_runtime(&runtime);
        }
    }

    /// Calls the callbacks of the HTTP requests that finished since the last
    /// frame. The outcome of a request whose resource has stopped meanwhile is
    /// dropped without touching the stopped runtime.
    pub(crate) fn deliver_http(&self) {
        for Completion {
            resource,
            callback,
            outcome,
        } in self.http.take_finished()
        {
            self.http.finish(&resource, callback.instance);
            let Some(runtime) = self.runtime_of(&callback) else {
                debug!("`{resource}` stopped before its HTTP request finished");
                continue;
            };
            if let Err(err) = runtime.call_ref(callback.id, &outcome.into_args()) {
                warn!("HTTP callback of `{resource}` failed: {err}");
            }
            runtime.release_ref(callback.id);
        }
    }

    /// Runs one frame of every running resource.
    pub(crate) fn tick_all(&self, now: Instant) {
        let snapshot = self.snapshot();
        for runtime in snapshot.iter() {
            runtime.tick(now);
        }
    }

    /// Runs the due timers of every running resource, without a frame.
    pub(crate) fn run_timers_all(&self, now: Instant) {
        let snapshot = self.snapshot();
        for runtime in snapshot.iter() {
            runtime.run_timers(now);
        }
    }

    /// The earliest timer deadline across running resources.
    pub(crate) fn next_deadline(&self) -> Option<Instant> {
        let snapshot = self.snapshot();
        snapshot
            .iter()
            .filter_map(|runtime| runtime.next_deadline())
            .min()
    }

    /// Removes and returns the next queued command line and its source.
    pub(crate) fn pop_command(&self) -> Option<(String, CommandSource)> {
        self.queued.borrow_mut().pop_front()
    }

    /// Drops every queued command line and returns how many there were.
    pub(crate) fn discard_commands(&self) -> usize {
        let mut queued = self.queued.borrow_mut();
        let count = queued.len();
        queued.clear();
        count
    }

    /// The script command called `name`, which is lower case.
    pub(crate) fn script_command(&self, name: &str) -> Option<ScriptCommand> {
        self.commands.borrow().get(name).cloned()
    }

    /// Every script command as its name and restricted flag, sorted by name.
    pub(crate) fn script_commands(&self) -> Vec<(String, bool)> {
        self.commands
            .borrow()
            .iter()
            .map(|(name, command)| (name.clone(), command.restricted))
            .collect()
    }

    fn ensure_scanned(&self) {
        if !self.scanned.get() {
            self.refresh();
        }
    }

    /// Runs `f` on the key-value store, opening it on first use. The store is
    /// rooted at the `sixm_kvpPath` convar, `cache/kvp` by default.
    fn with_kvp<T>(&self, f: impl FnOnce(&mut KvpStore) -> T) -> T {
        let mut slot = self.kvp.borrow_mut();
        let store = slot.get_or_insert_with(|| {
            let root = self
                .convar("sixm_kvpPath")
                .filter(|path| !path.is_empty())
                .unwrap_or_else(|| "cache/kvp".to_owned());
            KvpStore::new(root)
        });
        f(store)
    }

    /// Hands the key-value changes that are due to the writer thread. Called
    /// once per frame; a store that was never used costs nothing.
    pub(crate) fn flush_kvp_due(&self, now: Instant) {
        if let Some(store) = self.kvp.borrow_mut().as_mut() {
            store.flush_due(now);
        }
    }

    /// Writes every pending key-value change and stops the writer thread, at
    /// shutdown.
    pub(crate) fn shutdown_kvp(&self) {
        if let Some(store) = self.kvp.borrow_mut().as_mut() {
            store.shutdown();
        }
    }

    /// The running runtime called `name`.
    fn running_runtime(&self, name: &str) -> Option<Rc<LuaRuntime>> {
        self.running
            .borrow()
            .iter()
            .find(|runtime| runtime.resource().as_ref() == name)
            .cloned()
    }

    /// The running runtime that owns `func`, if this exact instance is still up.
    fn runtime_of(&self, func: &FuncRef) -> Option<Rc<LuaRuntime>> {
        self.running
            .borrow()
            .iter()
            .find(|runtime| {
                runtime.resource().as_ref() == func.owner.as_ref()
                    && runtime.instance() == func.instance
            })
            .cloned()
    }

    /// The running runtimes, in start order, as a shared slice that needs no borrow.
    pub(crate) fn snapshot(&self) -> Rc<[Rc<LuaRuntime>]> {
        self.snapshot.borrow().clone()
    }

    fn rebuild_snapshot(&self) {
        let fresh: Rc<[Rc<LuaRuntime>]> = self.running.borrow().iter().cloned().collect();
        *self.snapshot.borrow_mut() = fresh;
    }

    /// Delivers `event` to every running runtime that handles it, in start
    /// order. Returns whether a handler canceled it.
    pub(crate) fn dispatch(&self, event: &str, source: EventSource, args: &[Value]) -> bool {
        self.dispatch_from(event, source, args, None)
    }

    /// Like [`dispatch`](Self::dispatch), for an event that a resource started.
    /// `invoker` is what the handlers see as `GetInvokingResource()`.
    pub(crate) fn dispatch_from(
        &self,
        event: &str,
        source: EventSource,
        args: &[Value],
        invoker: Option<&ResourceName>,
    ) -> bool {
        let snapshot = self.snapshot();
        let ctx = invoker.map_or_else(EventContext::new, |name| {
            EventContext::invoked_by(name.clone())
        });
        for runtime in snapshot.iter() {
            if runtime.handles_event(event)
                && let Err(err) = runtime.deliver_event(event, source, args, &ctx)
            {
                error!(target: "sixm::script", "[{}] {event}: {err}", runtime.resource());
            }
        }
        ctx.is_canceled()
    }

    /// Fires a local event whose only argument is a resource name.
    fn dispatch_local(&self, event: &str, resource: &str) -> bool {
        self.dispatch(event, EventSource::Local, &[Value::from(resource)])
    }

    /// Creates the runtime for `name` and runs its scripts.
    fn start_one(&self, name: &ResourceName) -> Result<(), ManagerError> {
        if self.dispatch_local("onResourceStarting", name) {
            return Err(ManagerError::Canceled(name.to_string()));
        }
        let (root, exports, files) = self.script_plan(name)?;

        let instance = self.next_instance.get();
        self.next_instance.set(instance + 1);
        let runtime = LuaRuntime::new(name.clone(), instance, root, exports, self.me.clone())
            .map_err(|source| ManagerError::Runtime {
                resource: name.to_string(),
                source,
            })?;

        self.running.borrow_mut().push(Rc::clone(&runtime));
        self.rebuild_snapshot();
        self.advertise(name);

        self.set_transition(name, ResourceState::Starting);
        for file in &files {
            if let Err(err) = runtime.load_script(file) {
                error!("[{name}] cannot load {}: {err}", file.display());
            }
        }
        self.clear_transition(name);

        self.dispatch_local("onResourceStart", name);
        self.dispatch_local("onServerResourceStart", name);
        info!("Started resource {name}");
        Ok(())
    }

    /// Lists the client files of `name` to players. A resource whose files
    /// cannot be expanded is still listed, with no files, and the error is logged.
    fn advertise(&self, name: &ResourceName) {
        let resource = self.client_resource(name);
        let entry = listing(&resource);
        self.advertised.borrow_mut().push(resource);
        self.publish_advertised();
        self.with_server(|server| {
            if let Err(err) = server.broadcast_resource_started(entry) {
                warn!("cannot announce the start of `{name}`: {err}");
            }
        });
    }

    /// Removes `name` from the client files and tells players it stopped.
    fn withdraw(&self, name: &ResourceName) {
        self.advertised
            .borrow_mut()
            .retain(|resource| resource.name != name.as_ref());
        self.publish_advertised();
        self.with_server(|server| {
            if let Err(err) = server.broadcast_resource_stopped(name) {
                warn!("cannot announce the stop of `{name}`: {err}");
            }
        });
    }

    /// Hands the whole advertised list to the network. Does nothing while the
    /// network is not running; it is published when the network starts.
    pub(crate) fn publish_advertised(&self) {
        let resources = self.advertised.borrow().clone();
        self.with_server(|server| {
            if let Err(err) = server.publish_resources(resources) {
                warn!("cannot publish the client files: {err}");
            }
        });
    }

    /// The client files of `name`, hashed and ready to advertise.
    fn client_resource(&self, name: &ResourceName) -> PublishedResource {
        let catalog = self.catalog.borrow();
        let (files, exports, metadata) = match catalog.get(name) {
            Some(info) => (
                info.client_files(|dependency| {
                    catalog.get(dependency).map(|other| other.root.clone())
                }),
                info.manifest.exports.clone(),
                info.manifest.metadata.clone(),
            ),
            None => (Ok(Vec::new()), Vec::new(), Vec::new()),
        };
        let files = files.unwrap_or_else(|err| {
            error!("[{name}] cannot expand its client files, none are sent: {err}");
            Vec::new()
        });
        let exports = advertisable_exports(name, exports);
        let metadata = advertisable_metadata(name, metadata);
        let mut hashes = self.hashes.borrow_mut();
        PublishedResource {
            name: name.to_string(),
            files: files
                .into_iter()
                .filter_map(|file| hashes.publish(file))
                .collect(),
            exports,
            metadata,
        }
    }

    /// The root folder, server exports and script files of `name`.
    fn script_plan(
        &self,
        name: &ResourceName,
    ) -> Result<(PathBuf, Vec<String>, Vec<PathBuf>), ManagerError> {
        let catalog = self.catalog.borrow();
        let info = catalog
            .get(name)
            .ok_or_else(|| DependencyError::UnknownResource(name.to_string()))?;
        let files = info.server_script_files(|dependency| {
            catalog.get(dependency).map(|other| other.root.clone())
        })?;
        Ok((
            info.root.clone(),
            info.manifest.server_exports.clone(),
            files,
        ))
    }

    /// Stops the running dependents of `name`, then `name`. Returns the
    /// dependents that were stopped, in stop order.
    fn stop_with_dependents(&self, name: &str) -> Result<Vec<ResourceName>, ManagerError> {
        let target = self
            .running_runtime(name)
            .ok_or_else(|| ManagerError::NotRunning(name.to_owned()))?;
        let dependents: Vec<Rc<LuaRuntime>> = {
            let catalog = self.catalog.borrow();
            catalog
                .dependents_stop_order(name)
                .iter()
                .filter_map(|dependent| self.running_runtime(dependent))
                .collect()
        };
        let stopped = dependents
            .iter()
            .map(|runtime| runtime.resource().clone())
            .collect();
        for runtime in &dependents {
            self.stop_runtime(runtime);
        }
        self.stop_runtime(&target);
        Ok(stopped)
    }

    /// Fires the stop events, removes the runtime and its commands, and shuts it down.
    fn stop_runtime(&self, runtime: &Rc<LuaRuntime>) {
        let name = runtime.resource().clone();
        self.set_transition(&name, ResourceState::Stopping);
        self.dispatch_local("onResourceStop", &name);
        self.dispatch_local("onServerResourceStop", &name);
        self.release_commands_of(&name);
        self.running
            .borrow_mut()
            .retain(|candidate| !Rc::ptr_eq(candidate, runtime));
        self.rebuild_snapshot();
        self.clear_transition(&name);
        self.withdraw(&name);
        self.http.discard(&name, runtime.instance());
        runtime.shutdown();
        info!("Stopping resource {name}");
    }

    /// Records that `name` is in `state` until [`clear_transition`](Self::clear_transition).
    fn set_transition(&self, name: &ResourceName, state: ResourceState) {
        self.transitions.borrow_mut().push((name.clone(), state));
    }

    /// Removes the newest transition of `name`, if it has one.
    fn clear_transition(&self, name: &str) {
        let mut transitions = self.transitions.borrow_mut();
        if let Some(index) = transitions
            .iter()
            .rposition(|(owner, _)| owner.as_ref() == name)
        {
            transitions.remove(index);
        }
    }

    /// Removes the script commands that `owner` registered, releasing their handlers
    /// while `owner` is still running.
    fn release_commands_of(&self, owner: &str) {
        let removed: Vec<ScriptCommand> = {
            let mut commands = self.commands.borrow_mut();
            let keys: Vec<String> = commands
                .iter()
                .filter(|(_, command)| command.handler.owner.as_ref() == owner)
                .map(|(key, _)| key.clone())
                .collect();
            keys.iter().filter_map(|key| commands.remove(key)).collect()
        };
        for command in &removed {
            self.release_ref(&command.handler);
        }
    }
}

/// The client exports of `name` if clients can receive them: at most
/// [`MAX_EXPORTS`] names, each of 1 to [`MAX_EXPORT_NAME_LEN`] bytes. Otherwise
/// the error is logged and no exports are sent, rather than a part of them.
fn advertisable_exports(name: &ResourceName, exports: Vec<String>) -> Vec<String> {
    if exports.len() > MAX_EXPORTS {
        error!(
            "[{name}] declares {} exports, more than the {MAX_EXPORTS} limit, none are sent",
            exports.len()
        );
        return Vec::new();
    }
    if let Some(export) = exports
        .iter()
        .find(|export| export.is_empty() || export.len() > MAX_EXPORT_NAME_LEN)
    {
        error!(
            "[{name}] declares export `{export}`, which is not 1 to {MAX_EXPORT_NAME_LEN} bytes, none are sent"
        );
        return Vec::new();
    }
    exports
}

/// The metadata of `name` if clients can receive it: at most [`MAX_METADATA`]
/// pairs, keys of 1 to [`MAX_METADATA_KEY_LEN`] bytes and values of at most
/// [`MAX_METADATA_VALUE_LEN`] bytes. Otherwise the error is logged and no
/// metadata is sent.
fn advertisable_metadata(
    name: &ResourceName,
    metadata: Vec<(String, String)>,
) -> Vec<(String, String)> {
    if metadata.len() > MAX_METADATA {
        error!(
            "[{name}] declares {} metadata pairs, more than the {MAX_METADATA} limit, none are sent",
            metadata.len()
        );
        return Vec::new();
    }
    for (key, value) in &metadata {
        if key.is_empty() || key.len() > MAX_METADATA_KEY_LEN {
            error!(
                "[{name}] declares metadata key `{key}`, which is not 1 to {MAX_METADATA_KEY_LEN} bytes, none are sent"
            );
            return Vec::new();
        }
        if value.len() > MAX_METADATA_VALUE_LEN {
            error!(
                "[{name}] declares metadata `{key}` of {} bytes, more than {MAX_METADATA_VALUE_LEN}, none are sent",
                value.len()
            );
            return Vec::new();
        }
    }
    metadata
}

impl Host for ResourceManager {
    fn server(&self) -> Option<&dyn ServerHost> {
        Some(self)
    }

    fn client(&self) -> Option<&dyn ClientHost> {
        None
    }

    fn trigger_event(
        &self,
        event: &str,
        source: EventSource,
        args: &[Value],
        invoker: Option<&ResourceName>,
    ) -> Result<bool, ScriptError> {
        Ok(self.dispatch_from(event, source, args, invoker))
    }

    fn call_export(
        &self,
        resource: &str,
        export: &str,
        args: &[Value],
        invoker: Option<&ResourceName>,
    ) -> Result<Vec<Value>, ScriptError> {
        let runtime = self
            .running_runtime(resource)
            .ok_or_else(|| ScriptError::ResourceNotRunning(resource.to_owned()))?;
        runtime.call_export(export, args, invoker)
    }

    /// A transition or a running runtime wins over the catalog, so a running
    /// resource that a `refresh` dropped from the catalog is still `started`.
    fn resource_state(&self, resource: &str) -> ResourceState {
        let transitional = self
            .transitions
            .borrow()
            .iter()
            .rev()
            .find(|(owner, _)| owner.as_ref() == resource)
            .map(|(_, state)| *state);
        if let Some(state) = transitional {
            return state;
        }
        if self.is_running(resource) {
            ResourceState::Started
        } else if self.catalog.borrow().get(resource).is_some() {
            ResourceState::Stopped
        } else {
            ResourceState::Missing
        }
    }

    fn resource_names(&self) -> Vec<ResourceName> {
        self.catalog
            .borrow()
            .iter()
            .map(|info| info.name.clone())
            .collect()
    }

    fn resource_metadata(&self, resource: &str, key: &str) -> Vec<String> {
        self.catalog
            .borrow()
            .get(resource)
            .map(|info| {
                info.manifest
                    .metadata
                    .iter()
                    .filter(|(name, _)| name == key)
                    .map(|(_, value)| value.clone())
                    .collect()
            })
            .unwrap_or_default()
    }

    fn call_ref(&self, func: &FuncRef, args: &[Value]) -> Result<Vec<Value>, ScriptError> {
        if is_host(func) {
            let function = self
                .host_functions
                .borrow()
                .get(func)
                .ok_or_else(|| ScriptError::StaleRef(func.clone()))?;
            return function(args);
        }
        let runtime = self
            .runtime_of(func)
            .ok_or_else(|| ScriptError::StaleRef(func.clone()))?;
        runtime.call_ref(func.id, args)
    }

    fn retain_ref(&self, func: &FuncRef) -> bool {
        if is_host(func) {
            return self.host_functions.borrow().contains(func);
        }
        self.runtime_of(func)
            .is_some_and(|runtime| runtime.retain_ref(func.id))
    }

    fn perform_http_request(
        &self,
        resource: &ResourceName,
        request: HttpRequest,
        callback: FuncRef,
    ) -> Result<(), ScriptError> {
        let (timeout, workers) = {
            let convars = self.convars.borrow();
            (convars.http_timeout(), convars.http_workers())
        };
        let result = self
            .http
            .start(resource, request, callback.clone(), timeout, workers);
        if result.is_err() {
            self.release_ref(&callback);
        }
        result
    }

    fn release_ref(&self, func: &FuncRef) {
        if is_host(func) {
            // Host functions are removed by the server, not released by holders.
            return;
        }
        if let Some(runtime) = self.runtime_of(func) {
            runtime.release_ref(func.id);
        }
    }

    fn execute_command(&self, line: &str, invoker: Option<&ResourceName>) {
        let source = invoker.map_or(CommandSource::Console, |resource| {
            CommandSource::Resource(resource.clone())
        });
        self.queued
            .borrow_mut()
            .push_back((line.to_owned(), source));
    }

    fn register_command(&self, name: &str, handler: FuncRef, restricted: bool) {
        let key = name.to_ascii_lowercase();
        if !self.retain_ref(&handler) {
            warn!("command `{name}` ignored: its function is no longer valid");
            return;
        }
        let previous = self.commands.borrow_mut().insert(
            key,
            ScriptCommand {
                handler: handler.clone(),
                restricted,
            },
        );
        if let Some(previous) = previous {
            if previous.handler.owner != handler.owner {
                warn!(
                    "command `{name}` of `{}` replaced by `{}`",
                    previous.handler.owner, handler.owner
                );
            }
            self.release_ref(&previous.handler);
        }
    }

    fn get_convar(&self, name: &str) -> Option<String> {
        self.convar(name)
    }

    fn resource_path(&self, resource: &str) -> Option<PathBuf> {
        self.catalog
            .borrow()
            .get(resource)
            .map(|info| info.root.clone())
    }

    fn load_resource_file(&self, resource: &str, path: &str) -> Option<Vec<u8>> {
        let root = self.resource_path(resource)?;
        std::fs::read(root.join(path)).ok()
    }

    fn save_resource_file(&self, resource: &str, path: &str, data: &[u8]) -> bool {
        let Some(root) = self.resource_path(resource) else {
            return false;
        };
        let full = root.join(path);
        if let Some(parent) = full.parent()
            && std::fs::create_dir_all(parent).is_err()
        {
            return false;
        }
        std::fs::write(full, data).is_ok()
    }

    fn kvp_get(&self, resource: &str, key: &str) -> Option<Vec<u8>> {
        self.with_kvp(|store| store.get(resource, key))
    }

    fn kvp_set(&self, resource: &str, key: &str, value: &[u8]) -> bool {
        self.with_kvp(|store| store.set(resource, key, value))
    }

    fn kvp_delete(&self, resource: &str, key: &str) -> bool {
        self.with_kvp(|store| store.delete(resource, key))
    }

    fn kvp_keys(&self, resource: &str, prefix: &str) -> Vec<String> {
        self.with_kvp(|store| store.keys(resource, prefix))
    }

    fn kvp_flush(&self) {
        self.with_kvp(KvpStore::flush);
    }

    fn state_get(&self, bag: &str, key: &str) -> Option<Value> {
        self.state.borrow().get(bag, key).cloned()
    }

    fn state_set(
        &self,
        bag: &str,
        key: &str,
        value: Value,
        replicated: bool,
    ) -> Result<(), ScriptError> {
        self.write_state(bag, key, &value, replicated)
    }

    fn entity(&self, entity: u32) -> Option<EntityInfo> {
        self.entities.borrow().get(entity)
    }

    fn entities(&self, kind: Option<EntityKind>) -> Vec<u32> {
        self.entities.borrow().ids(kind)
    }
}

/// Checks that a replicated convar fits the wire: the name is 1 to
/// [`MAX_CONVAR_NAME_LEN`] bytes and the value at most [`MAX_CONVAR_VALUE_LEN`].
fn check_replicated(name: &str, value: &str) -> Result<(), ScriptError> {
    if name.len() > MAX_CONVAR_NAME_LEN {
        return Err(ScriptError::Runtime(format!(
            "a replicated convar name must be at most {MAX_CONVAR_NAME_LEN} bytes"
        )));
    }
    if value.len() > MAX_CONVAR_VALUE_LEN {
        return Err(ScriptError::Runtime(format!(
            "a replicated convar value must be at most {MAX_CONVAR_VALUE_LEN} bytes"
        )));
    }
    Ok(())
}

impl ServerHost for ResourceManager {
    fn set_convar(&self, name: &str, value: &str, kind: ConvarKind) -> Result<(), ScriptError> {
        self.set_convar(name, value, kind)
    }

    fn trigger_client_event(
        &self,
        event: &str,
        target: ClientTarget,
        args: &[Value],
    ) -> Result<(), ScriptError> {
        self.send_client_event(event, target, args)
    }

    fn trigger_latent_client_event(
        &self,
        event: &str,
        target: ClientTarget,
        args: &[Value],
        bps: u64,
    ) -> Result<(), ScriptError> {
        self.send_latent_client_event(event, target, args, bps)
    }

    fn players(&self) -> Vec<u32> {
        self.player_ids()
    }

    fn player_info(&self, player: u32) -> Option<PlayerInfo> {
        self.player_details(player)
    }

    fn player_ping(&self, player: u32) -> Option<std::time::Duration> {
        self.player_round_trip(player)
    }

    fn player_identifiers(&self, player: u32) -> Vec<String> {
        self.identifiers_of(player)
    }

    fn drop_player(&self, player: u32, reason: &str) {
        self.queue_drop(player, reason);
    }

    fn is_ace_allowed(&self, principal: &str, object: &str) -> bool {
        self.ace_allowed(principal, object)
    }

    fn create_entity(&self, info: EntityInfo) -> Result<u32, ScriptError> {
        self.spawn_entity(info)
    }

    fn delete_entity(&self, entity: u32) -> bool {
        self.remove_entity(entity)
    }

    fn set_entity_position(&self, entity: u32, position: [f32; 3]) -> Result<(), ScriptError> {
        self.entities.borrow_mut().set_position(entity, position)
    }

    fn set_entity_rotation(&self, entity: u32, rotation: [f32; 3]) -> Result<(), ScriptError> {
        self.entities.borrow_mut().set_rotation(entity, rotation)
    }

    fn player_ped(&self, player: u32) -> Option<u32> {
        self.ped_of_player(player)
    }

    fn player_routing_bucket(&self, player: u32) -> Option<u32> {
        self.routed_player(player)
    }

    fn set_player_routing_bucket(&self, player: u32, bucket: u32) -> bool {
        self.route_player(player, bucket)
    }

    fn entity_routing_bucket(&self, entity: u32) -> Option<u32> {
        self.routed_entity(entity)
    }

    fn set_entity_routing_bucket(&self, entity: u32, bucket: u32) -> bool {
        self.route_entity(entity, bucket)
    }

    fn players_in_routing_bucket(&self, bucket: u32) -> Vec<u32> {
        self.players_in_bucket(bucket)
    }

    fn set_routing_bucket_population_enabled(&self, bucket: u32, enabled: bool) {
        self.bucket_settings
            .borrow_mut()
            .set_population(bucket, enabled);
    }

    fn routing_bucket_population_enabled(&self, bucket: u32) -> bool {
        self.bucket_settings.borrow().population_enabled(bucket)
    }

    fn set_routing_bucket_entity_lockdown_mode(
        &self,
        bucket: u32,
        mode: &str,
    ) -> Result<(), ScriptError> {
        self.bucket_settings.borrow_mut().set_lockdown(bucket, mode)
    }

    fn set_entity_owner(&self, entity: u32, owner: u32) -> bool {
        self.script_set_owner(entity, owner)
    }

    fn set_entity_orphan_mode(&self, entity: u32, mode: &str) -> Result<(), ScriptError> {
        self.script_set_orphan_mode(entity, mode)
    }

    fn routing_bucket_entity_lockdown_mode(&self, bucket: u32) -> String {
        if let Some(mode) = self.bucket_settings.borrow().lockdown_mode(bucket) {
            return mode.to_owned();
        }
        self.convars.borrow().entity_lockdown().to_owned()
    }
}
