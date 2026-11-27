//! The resource manager: installed and running resources, script commands, and
//! the [`Host`] that script runtimes call back into.

use std::borrow::Cow;
use std::cell::{Cell, RefCell};
use std::collections::{BTreeMap, VecDeque};
use std::path::{Path, PathBuf};
use std::rc::{Rc, Weak};
use std::time::Instant;

use serde_json::json;

use sixm_core::{
    ClientHost, ClientTarget, ConvarKind, EntityAttachment, EntityInfo, EntityKind, EntityState,
    EventContext, EventSource, FuncRef, Host, HttpRequest, MAX_RESOURCE_FILE_WRITE_BYTES,
    PlayerInfo, ResourceDirectory, ResourceName, ResourceState, ScriptError, ScriptLanguage,
    ScriptRuntime, ServerHost, Value, WebReply, is_protected_resource_file,
};
use sixm_js::JsRuntime;
use sixm_kvp::KvpStore;
use sixm_lua::LuaRuntime;
use sixm_net::{
    EntityStateUpdate, MAX_CONVAR_NAME_LEN, MAX_CONVAR_VALUE_LEN, MAX_EXPORT_NAME_LEN, MAX_EXPORTS,
    MAX_METADATA, MAX_METADATA_KEY_LEN, MAX_METADATA_VALUE_LEN, PublishedResource, Traffic,
};
use sixm_resources::{Catalog, DependencyError, ResourceError, ScanIssue};
use tracing::{debug, error, info, warn};

use crate::access::AccessStore;
use crate::buckets::BucketSettings;
use crate::commands::CommandSource;
use crate::convars::{Convars, KVP_PATH, RCON_PASSWORD};
use crate::entities::{EntityStore, EntityUpdate};
use crate::host_functions::{HostFunctions, is_host};
use crate::http::{Completion, HttpClient};
use crate::network::{DROP_SCRIPT, DropOrigin, Network};
use crate::perf::FrameHistory;
use crate::profiler::Profiler;
use crate::resource_files::{FileHashes, listing};
use crate::scope::Scopes;
use crate::state::StateStore;
use crate::watch::ResourceWatcher;
use crate::web::WebState;

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

/// The script runtimes of one running resource, in load order.
#[derive(Debug)]
struct RunningResource {
    name: ResourceName,
    runtimes: Vec<Rc<dyn ScriptRuntime>>,
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
    running: RefCell<Vec<RunningResource>>,
    /// The running runtimes as one shared slice, rebuilt when `running` changes.
    /// Event fan-out and frames clone this `Rc` instead of allocating.
    snapshot: RefCell<Rc<[Rc<dyn ScriptRuntime>]>>,
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
    /// Reused between frames to hold the delta entity state updates sent to clients.
    pub(crate) entity_state_updates: RefCell<Vec<EntityStateUpdate>>,
    /// Access control entries and principal links, changed by the `add_ace` family.
    pub(crate) access: RefCell<AccessStore>,
    /// Functions the server offers to scripts, such as `setKickReason`.
    pub(crate) host_functions: RefCell<HostFunctions>,
    /// Durations of the recent frames, for the `perf` command.
    pub(crate) frames: RefCell<FrameHistory>,
    /// Fast guard for whether `profiler` recording is active (zero idle overhead).
    pub(crate) profiler_active: Cell<bool>,
    /// Chrome trace frame & resource profiler state.
    pub(crate) profiler: RefCell<Profiler>,
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
    /// HTTP handlers of the resources and the requests they are answering.
    pub(crate) web: WebState,
    /// The key-value store of the resources, opened on first use in `sixm_kvpPath`.
    kvp: RefCell<Option<KvpStore>>,
    /// Whether hot-reload watching is enabled (`--watch` or `watch_resources on`).
    watch_enabled: Cell<bool>,
    /// File modification tracker for running resources when watch is enabled.
    watcher: RefCell<ResourceWatcher>,
    /// When the last master heartbeat was queued.
    last_heartbeat: Cell<Option<Instant>>,
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
            entity_state_updates: RefCell::default(),
            access: RefCell::default(),
            host_functions: RefCell::default(),
            frames: RefCell::default(),
            profiler_active: Cell::new(false),
            profiler: RefCell::default(),
            perf_mark: Cell::new((Instant::now(), Traffic::default())),
            advertised: RefCell::default(),
            hashes: RefCell::default(),
            transitions: RefCell::default(),
            http: HttpClient::default(),
            web: WebState::default(),
            kvp: RefCell::default(),
            watch_enabled: Cell::new(false),
            watcher: RefCell::default(),
            last_heartbeat: Cell::new(None),
        })
    }

    /// Resolves `name` through the catalog's `provide` aliases to its canonical
    /// installed resource name, or returns `name` unchanged if unknown/canonical.
    fn resolve_resource<'a>(&self, name: &'a str) -> Cow<'a, str> {
        let catalog = self.catalog.borrow();
        let resolved = catalog.resolve_name(name);
        if resolved == name {
            Cow::Borrowed(name)
        } else {
            Cow::Owned(resolved.to_owned())
        }
    }

    /// Whether canonical resource name `canonical` is currently running.
    fn is_running_canonical(&self, canonical: &str) -> bool {
        self.running
            .borrow()
            .iter()
            .any(|res| res.name.as_ref() == canonical)
    }

    /// Starts `name`, after any of its dependencies that are not running yet.
    ///
    /// A resource that fails to load its scripts is logged and the rest still
    /// load. A canceled or unloadable dependency stops the chain.
    pub fn start(&self, name: &str) -> Result<(), ManagerError> {
        self.ensure_scanned();
        let canonical = self.resolve_resource(name);
        if self.is_running_canonical(&canonical) {
            info!("Resource {canonical} is already running");
            return Ok(());
        }
        let order = self.catalog.borrow().start_order(&canonical)?;
        for resource in &order {
            if !self.is_running_canonical(resource) {
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
        let canonical = self.resolve_resource(name);
        if !self.is_running_canonical(&canonical) {
            return self.start(&canonical);
        }
        let dependents = self.stop_with_dependents(&canonical)?;
        self.start(&canonical)?;
        for dependent in dependents.iter().rev() {
            self.start(dependent)?;
        }
        Ok(())
    }

    /// Restarts `name` if it is running, and starts it otherwise.
    pub fn ensure(&self, name: &str) -> Result<(), ManagerError> {
        self.ensure_scanned();
        let canonical = self.resolve_resource(name);
        if self.is_running_canonical(&canonical) {
            self.restart(&canonical)
        } else {
            self.start(&canonical)
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
        self.dispatch("onResourceListRefresh", EventSource::Local, &[]);
    }

    /// Whether `name` (canonical name or `provide` alias) is running.
    pub fn is_running(&self, name: &str) -> bool {
        let canonical = self.resolve_resource(name);
        self.is_running_canonical(&canonical)
    }

    /// Whether hot-reload resource watching is enabled.
    pub fn watch_enabled(&self) -> bool {
        self.watch_enabled.get()
    }

    /// Enables or disables hot-reload resource watching, resetting cached
    /// file baselines.
    pub fn set_watch_enabled(&self, enabled: bool) {
        self.watch_enabled.set(enabled);
        self.watcher.borrow_mut().reset();
    }

    /// Polls running resources for manifest/script changes when watching is
    /// enabled and the poll interval has elapsed, calling `refresh` and
    /// `restart` on changed resources.
    pub(crate) fn poll_watch(&self, now: Instant) {
        if !self.watch_enabled.get() {
            return;
        }
        if !self.watcher.borrow_mut().poll_due(now) {
            return;
        }
        let running = self.running_roots();
        let changed = self.watcher.borrow_mut().detect_changes(&running);
        if changed.is_empty() {
            return;
        }
        self.refresh();
        for resource in changed {
            if self.is_running_canonical(&resource) {
                info!("watch: reloading changed resource `{resource}`");
                if let Err(err) = self.restart(&resource) {
                    error!("watch: failed to restart `{resource}`: {err}");
                }
            }
        }
        let updated = self.running_roots();
        self.watcher.borrow_mut().seed(&updated);
    }

    fn running_roots(&self) -> Vec<(ResourceName, PathBuf)> {
        let names = self.running_resources();
        let catalog = self.catalog.borrow();
        names
            .into_iter()
            .filter_map(|name| {
                catalog
                    .get(name.as_ref())
                    .map(|info| (name, info.root.clone()))
            })
            .collect()
    }

    /// Sends a periodic discovery heartbeat if `sv_masterUrl` is configured.
    pub(crate) fn tick_heartbeat(&self, now: Instant) {
        let (url, interval, timeout, workers, allow_private) = {
            let convars = self.convars.borrow();
            let Some(url) = convars.master_url().map(str::to_owned) else {
                return;
            };
            (
                url,
                convars.heartbeat_interval(),
                convars.http_timeout(),
                convars.http_workers(),
                convars.http_allow_private(),
            )
        };
        if let Some(last) = self.last_heartbeat.get()
            && now.saturating_duration_since(last) < interval
        {
            return;
        }
        self.last_heartbeat.set(Some(now));
        let payload = json!({
            "info": self.info_document(),
            "dynamic": self.dynamic_document(),
        });
        let Ok(body) = serde_json::to_vec(&payload) else {
            return;
        };
        if let Err(err) =
            self.http
                .post_fire_and_forget(&url, body, timeout, workers, allow_private)
        {
            debug!("master heartbeat to `{url}` skipped: {err}");
        }
    }

    /// Names of the running resources, in start order.
    pub fn running_resources(&self) -> Vec<ResourceName> {
        self.running
            .borrow()
            .iter()
            .map(|res| res.name.clone())
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
        if name.eq_ignore_ascii_case(RCON_PASSWORD) {
            if kind != ConvarKind::Plain {
                return Err(ScriptError::Runtime("use set for rcon_password".to_owned()));
            }
            self.convars.borrow_mut().set_rcon_password(value);
            return Ok(());
        }
        let key = name.to_ascii_lowercase();
        if kind != ConvarKind::Plain && self.convars.borrow().is_restricted(&key) {
            return Err(ScriptError::Runtime(
                "restricted convars must remain plain".to_owned(),
            ));
        }
        if kind == ConvarKind::Replicated {
            check_replicated(name, value)?;
        }
        let previous_value = self.convars.borrow().get(name).map(str::to_owned);
        let previous = self.convars.borrow_mut().set(name, value, kind);
        match (kind, previous) {
            (ConvarKind::Replicated, _) => self.broadcast_convar(&key, Some(value)),
            (_, Some(ConvarKind::Replicated)) => self.broadcast_convar(&key, None),
            _ => {}
        }
        if previous_value.as_deref() != Some(value) {
            let replicated = kind == ConvarKind::Replicated;
            for runtime in self.snapshot().iter() {
                let can_read = self.convars.borrow().can_read(&key, runtime.resource());
                if can_read {
                    runtime.deliver_convar_change(name, Some(value), replicated);
                }
            }
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
            let last = self.running.borrow().last().map(|res| res.name.clone());
            let Some(name) = last else {
                return;
            };
            self.stop_resource(&name);
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
        let profiling = self.profiler_active.get();
        let snapshot = self.snapshot();
        for runtime in snapshot.iter() {
            if profiling {
                let start = Instant::now();
                runtime.tick(now);
                self.profiler.borrow_mut().record_span(
                    runtime.resource(),
                    "resource",
                    start,
                    start.elapsed(),
                );
            } else {
                runtime.tick(now);
            }
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
                .convar(KVP_PATH)
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

    /// The stored `ResourceName` of `name`, if it is running.
    fn running_name(&self, name: &str) -> Option<ResourceName> {
        self.running
            .borrow()
            .iter()
            .find(|res| res.name.as_ref() == name)
            .map(|res| res.name.clone())
    }

    /// The running runtimes of `name`, in load order.
    fn running_runtimes(&self, name: &str) -> Option<Vec<Rc<dyn ScriptRuntime>>> {
        self.running
            .borrow()
            .iter()
            .find(|res| res.name.as_ref() == name)
            .map(|res| res.runtimes.clone())
    }

    /// The running runtime that owns `func`, if this exact instance is still up.
    pub(crate) fn runtime_of(&self, func: &FuncRef) -> Option<Rc<dyn ScriptRuntime>> {
        self.running
            .borrow()
            .iter()
            .find(|res| res.name.as_ref() == func.owner.as_ref())
            .and_then(|res| {
                res.runtimes
                    .iter()
                    .find(|runtime| runtime.instance() == func.instance)
                    .cloned()
            })
    }

    /// The running runtimes, in start order, as a shared slice that needs no borrow.
    pub(crate) fn snapshot(&self) -> Rc<[Rc<dyn ScriptRuntime>]> {
        self.snapshot.borrow().clone()
    }

    fn rebuild_snapshot(&self) {
        let fresh: Rc<[Rc<dyn ScriptRuntime>]> = self
            .running
            .borrow()
            .iter()
            .flat_map(|res| res.runtimes.iter().cloned())
            .collect();
        *self.snapshot.borrow_mut() = fresh;
    }

    /// Delivers `event` to every running runtime that handles it, in start
    /// order. Returns whether a handler canceled it.
    pub(crate) fn dispatch(&self, event: &str, source: EventSource, args: &[Value]) -> bool {
        self.dispatch_from(event, source, args, None)
    }

    /// Whether any running runtime handles `event`. Lets the callers skip
    /// building the arguments of an event nobody listens to.
    pub(crate) fn has_handler(&self, event: &str) -> bool {
        self.snapshot()
            .iter()
            .any(|runtime| runtime.handles_event(event))
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
        let scripts: Vec<_> = files
            .iter()
            .filter_map(|file| {
                let Some(language) = ScriptLanguage::from_path(file) else {
                    warn!(
                        "[{name}] skipping unsupported script type: {}",
                        file.display()
                    );
                    return None;
                };
                Some((file, language))
            })
            .collect();

        let (runtimes, lua_rt, js_rt) = self
            .create_runtimes(
                name,
                &root,
                &exports,
                scripts.iter().map(|(_, language)| *language),
            )
            .map_err(|source| ManagerError::Runtime {
                resource: name.to_string(),
                source,
            })?;

        self.running.borrow_mut().push(RunningResource {
            name: name.clone(),
            runtimes,
        });
        self.rebuild_snapshot();
        self.advertise(name);

        self.set_transition(name, ResourceState::Starting);
        for (file, language) in scripts {
            let result = match language {
                ScriptLanguage::JavaScript => js_rt.as_ref().map(|rt| rt.load_script(file)),
                ScriptLanguage::Lua => lua_rt.as_ref().map(|rt| rt.load_script(file)),
            };
            if let Some(Err(err)) = result {
                error!("[{name}] cannot load {}: {err}", file.display());
            }
        }
        self.clear_transition(name);

        self.dispatch_local("onResourceStart", name);
        self.dispatch_local("onServerResourceStart", name);
        info!("Started resource {name}");
        Ok(())
    }

    /// Creates one runtime per language, in order of first appearance.
    #[expect(clippy::type_complexity)]
    fn create_runtimes(
        &self,
        name: &ResourceName,
        root: &Path,
        exports: &[String],
        languages: impl IntoIterator<Item = ScriptLanguage>,
    ) -> Result<
        (
            Vec<Rc<dyn ScriptRuntime>>,
            Option<Rc<LuaRuntime>>,
            Option<Rc<JsRuntime>>,
        ),
        ScriptError,
    > {
        let mut runtimes: Vec<Rc<dyn ScriptRuntime>> = Vec::new();
        let mut lua_rt: Option<Rc<LuaRuntime>> = None;
        let mut js_rt: Option<Rc<JsRuntime>> = None;

        for language in languages {
            if language == ScriptLanguage::JavaScript {
                if js_rt.is_none() {
                    let instance = self.next_instance.get();
                    self.next_instance.set(instance + 1);
                    let rt = JsRuntime::new(
                        name.clone(),
                        instance,
                        root.to_path_buf(),
                        exports.to_vec(),
                        self.me.clone(),
                    )?;
                    runtimes.push(Rc::clone(&rt) as Rc<dyn ScriptRuntime>);
                    js_rt = Some(rt);
                }
            } else if lua_rt.is_none() {
                let instance = self.next_instance.get();
                self.next_instance.set(instance + 1);
                let rt = LuaRuntime::new(
                    name.clone(),
                    instance,
                    root.to_path_buf(),
                    exports.to_vec(),
                    self.me.clone(),
                )?;
                runtimes.push(Rc::clone(&rt) as Rc<dyn ScriptRuntime>);
                lua_rt = Some(rt);
            }
        }
        Ok((runtimes, lua_rt, js_rt))
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
        let canonical = self.resolve_resource(name);
        let target = self
            .running_name(&canonical)
            .ok_or_else(|| ManagerError::NotRunning(name.to_owned()))?;
        let dependents: Vec<ResourceName> = {
            let catalog = self.catalog.borrow();
            catalog
                .dependents_stop_order(&canonical)
                .iter()
                .filter_map(|dependent| self.running_name(dependent))
                .collect()
        };
        for dependent in &dependents {
            self.stop_resource(dependent);
        }
        self.stop_resource(&target);
        Ok(dependents)
    }

    /// Fires the stop events, removes the resource's runtimes and commands, and shuts them down.
    fn stop_resource(&self, name: &ResourceName) {
        self.set_transition(name, ResourceState::Stopping);
        self.dispatch_local("onResourceStop", name);
        self.dispatch_local("onServerResourceStop", name);
        self.release_commands_of(name);
        self.forget_web(name);
        let removed = {
            let mut running = self.running.borrow_mut();
            running
                .iter()
                .position(|res| res.name == *name)
                .map(|index| running.remove(index))
        };
        self.rebuild_snapshot();
        self.clear_transition(name);
        self.withdraw(name);
        if let Some(resource) = removed {
            for runtime in resource.runtimes.into_iter().rev() {
                self.http.discard(name, runtime.instance());
                runtime.shutdown();
            }
        }
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
        let canonical = self.resolve_resource(resource);
        let runtimes = self
            .running_runtimes(&canonical)
            .ok_or_else(|| ScriptError::ResourceNotRunning(resource.to_owned()))?;
        for runtime in &runtimes {
            match runtime.call_export(export, args, invoker) {
                Ok(values) => return Ok(values),
                Err(ScriptError::NoSuchExport { .. }) => {}
                Err(err) => return Err(err),
            }
        }
        Err(ScriptError::NoSuchExport {
            resource: resource.to_owned(),
            export: export.to_owned(),
        })
    }

    /// A transition or a running runtime wins over the catalog, so a running
    /// resource that a `refresh` dropped from the catalog is still `started`.
    fn resource_state(&self, resource: &str) -> ResourceState {
        let canonical = self.resolve_resource(resource);
        let transitional = self
            .transitions
            .borrow()
            .iter()
            .rev()
            .find(|(owner, _)| owner.as_ref() == canonical.as_ref())
            .map(|(_, state)| *state);
        if let Some(state) = transitional {
            return state;
        }
        if self.is_running_canonical(&canonical) {
            ResourceState::Started
        } else if self.catalog.borrow().get(&canonical).is_some() {
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
        let canonical = self.resolve_resource(resource);
        self.catalog
            .borrow()
            .get(&canonical)
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
        let (timeout, workers, allow_private) = {
            let convars = self.convars.borrow();
            (
                convars.http_timeout(),
                convars.http_workers(),
                convars.http_allow_private(),
            )
        };
        let result = self.http.start(
            resource,
            request,
            callback.clone(),
            timeout,
            workers,
            allow_private,
        );
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

    fn get_resource_convar(&self, resource: &str, name: &str) -> Option<String> {
        self.convars
            .borrow()
            .get_for_resource(name, resource)
            .map(str::to_owned)
    }

    fn resource_path(&self, resource: &str) -> Option<PathBuf> {
        let canonical = self.resolve_resource(resource);
        self.catalog
            .borrow()
            .get(&canonical)
            .map(|info| info.root.clone())
    }

    fn load_resource_file(&self, resource: &str, path: &str) -> Option<Vec<u8>> {
        let root = self.resource_path(resource)?;
        ResourceDirectory::open(&root).ok()?.read(path).ok()
    }

    fn save_resource_file(&self, resource: &str, path: &str, data: &[u8]) -> bool {
        if is_protected_resource_file(path) {
            return false;
        }
        let Some(root) = self.resource_path(resource) else {
            return false;
        };
        ResourceDirectory::open(&root)
            .and_then(|directory| {
                directory.write_bounded(path, data, MAX_RESOURCE_FILE_WRITE_BYTES, None)
            })
            .is_ok()
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

    fn state_keys(&self, bag: &str) -> Option<Vec<String>> {
        self.state_bag_keys(bag)
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

    fn entity_state(&self, entity: u32) -> Option<EntityState> {
        self.entities.borrow().state(entity)
    }

    fn entities(&self, kind: Option<EntityKind>) -> Vec<u32> {
        self.entities.borrow().ids(kind)
    }

    fn registered_commands(&self) -> Vec<String> {
        let mut names: Vec<String> = crate::commands::BUILT_IN
            .iter()
            .map(|name| (*name).to_owned())
            .collect();
        names.extend(self.commands.borrow().keys().cloned());
        names.sort_unstable();
        names.dedup();
        names
    }

    fn resource_commands(&self, resource: &str) -> Vec<String> {
        self.commands
            .borrow()
            .iter()
            .filter(|(_, command)| command.handler.owner.as_ref() == resource)
            .map(|(name, _)| name.clone())
            .collect()
    }

    fn entity_script(&self, entity: u32) -> Option<String> {
        self.entities.borrow().script(entity)
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
        if Convars::is_console_only(name) {
            return Err(ScriptError::Runtime(format!(
                "{} is console-only",
                name.to_ascii_lowercase()
            )));
        }
        self.set_convar(name, value, kind)
    }

    fn set_convar_for_resource(
        &self,
        resource: &str,
        name: &str,
        value: &str,
        kind: ConvarKind,
    ) -> Result<(), ScriptError> {
        const MAX_SCRIPT_CONVAR_VALUE_LEN: usize = 64 * 1024;
        if name.len() > MAX_CONVAR_NAME_LEN || value.len() > MAX_SCRIPT_CONVAR_VALUE_LEN {
            return Err(ScriptError::Runtime(
                "convar name or value exceeds script size limits".to_owned(),
            ));
        }
        if Convars::is_console_only(name) {
            return Err(ScriptError::Runtime(format!(
                "{} is console-only",
                name.to_ascii_lowercase()
            )));
        }
        let key = name.to_ascii_lowercase();
        if !self.convars.borrow().can_read(&key, resource) {
            return Err(ScriptError::Runtime(format!(
                "resource `{resource}` cannot modify restricted convar `{name}`"
            )));
        }
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

    fn drop_player(&self, player: u32, reason: &str, resource: &str) {
        let origin = DropOrigin {
            resource: resource.to_owned(),
            code: DROP_SCRIPT,
        };
        self.queue_drop(player, reason, origin);
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

    fn set_web_handler(&self, resource: &ResourceName, handler: FuncRef) {
        self.install_web_handler(resource, handler);
    }

    fn web_reply(&self, request: u64, reply: WebReply) -> Result<(), ScriptError> {
        self.reply_web(request, reply)
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

    fn set_entity_frozen(&self, entity: u32, frozen: bool) -> Result<(), ScriptError> {
        self.entities.borrow_mut().set_frozen(entity, frozen)
    }

    fn set_entity_velocity(&self, entity: u32, velocity: [f32; 3]) -> Result<(), ScriptError> {
        self.entities.borrow_mut().set_velocity(entity, velocity)
    }

    fn set_entity_health(&self, entity: u32, health: i32) -> Result<(), ScriptError> {
        self.entities.borrow_mut().set_health(entity, health)
    }

    fn set_entity_max_health(&self, entity: u32, max_health: i32) -> Result<(), ScriptError> {
        self.entities
            .borrow_mut()
            .set_max_health(entity, max_health)
    }

    fn set_entity_visible(&self, entity: u32, visible: bool) -> Result<(), ScriptError> {
        self.entities.borrow_mut().set_visible(entity, visible)
    }

    fn set_entity_alpha(&self, entity: u32, alpha: u8) -> Result<(), ScriptError> {
        self.entities.borrow_mut().set_alpha(entity, alpha)
    }

    fn attach_entity_to_entity(
        &self,
        entity: u32,
        attachment: EntityAttachment,
    ) -> Result<(), ScriptError> {
        self.entities.borrow_mut().attach(entity, attachment)
    }

    fn detach_entity(&self, entity: u32) -> Result<(), ScriptError> {
        self.entities.borrow_mut().detach(entity)
    }

    fn start_resource(&self, caller: &str, resource: &str) -> bool {
        if !self.resource_may(caller, "command.start") {
            warn!("resource `{caller}` may not run `StartResource`");
            return false;
        }
        if self.is_running(resource) {
            return false;
        }
        match self.start(resource) {
            Ok(()) => self.is_running(resource),
            Err(err) => {
                error!("StartResource `{resource}` failed: {err}");
                false
            }
        }
    }

    fn stop_resource(&self, caller: &str, resource: &str) -> bool {
        if !self.resource_may(caller, "command.stop") {
            warn!("resource `{caller}` may not run `StopResource`");
            return false;
        }
        if !self.is_running(resource) {
            return false;
        }
        match self.stop(resource) {
            Ok(()) => !self.is_running(resource),
            Err(err) => {
                error!("StopResource `{resource}` failed: {err}");
                false
            }
        }
    }

    fn restart_resource(&self, caller: &str, resource: &str) -> bool {
        if !self.resource_may(caller, "command.restart") {
            warn!("resource `{caller}` may not run `RestartResource`");
            return false;
        }
        match self.restart(resource) {
            Ok(()) => true,
            Err(err) => {
                error!("RestartResource `{resource}` failed: {err}");
                false
            }
        }
    }

    fn entity_first_owner(&self, entity: u32) -> Option<u32> {
        self.entities.borrow().creator(entity)
    }

    fn set_entity_cull_radius(&self, entity: u32, radius: f32) -> Result<(), ScriptError> {
        self.entities.borrow_mut().set_cull_radius(entity, radius)
    }

    fn set_entity_script(&self, entity: u32, script: &str) {
        self.entities.borrow_mut().set_script(entity, script);
    }

    fn player_last_msg(&self, player: u32) -> Option<std::time::Duration> {
        self.player_last_message(player)
    }

    fn player_tokens(&self, player: u32) -> Vec<String> {
        self.player_tokens_of(player)
    }

    fn player_time_online(&self, player: u32) -> Option<std::time::Duration> {
        self.player_online_for(player)
    }

    fn routing_bucket_entity_lockdown_mode(&self, bucket: u32) -> String {
        if let Some(mode) = self.bucket_settings.borrow().lockdown_mode(bucket) {
            return mode.to_owned();
        }
        self.convars.borrow().entity_lockdown().to_owned()
    }
}
