//! The resource manager: installed and running resources, script commands, and
//! the [`Host`] that script runtimes call back into.

use std::cell::{Cell, RefCell};
use std::collections::{BTreeMap, VecDeque};
use std::path::PathBuf;
use std::rc::{Rc, Weak};
use std::time::Instant;

use sixm_core::{
    ClientHost, ClientTarget, EventContext, EventSource, FuncRef, Host, PlayerInfo, ResourceName,
    ScriptError, ScriptRuntime, ServerHost, Value,
};
use sixm_lua::LuaRuntime;
use sixm_net::{MAX_EXPORT_NAME_LEN, MAX_EXPORTS, PublishedResource};
use sixm_resources::{Catalog, DependencyError, ResourceError, ScanIssue};
use tracing::{error, info, warn};

use crate::convars::Convars;
use crate::host_functions::{HostFunctions, is_host};
use crate::network::Network;
use crate::resource_files::{FileHashes, listing};
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
    /// Console lines queued by `ExecuteCommand`, run after the current call.
    queued: RefCell<VecDeque<String>>,
    quit: Cell<bool>,
    /// The listening socket, connected players and pending kicks.
    pub(crate) network: Network,
    /// State bags and their keys.
    pub(crate) state: RefCell<StateStore>,
    /// Functions the server offers to scripts, such as `setKickReason`.
    pub(crate) host_functions: RefCell<HostFunctions>,
    /// Client files of the running resources, in start order. Advertised to
    /// players as a whole whenever it changes.
    advertised: RefCell<Vec<PublishedResource>>,
    /// Hashes of the client files, so that restarts do not read them again.
    hashes: RefCell<FileHashes>,
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
            host_functions: RefCell::default(),
            advertised: RefCell::default(),
            hashes: RefCell::default(),
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

    /// Sets a convar. Names are case-insensitive.
    pub fn set_convar(&self, name: &str, value: &str) {
        self.convars.borrow_mut().set(name, value);
    }

    /// The value of a convar, if it is set.
    pub fn convar(&self, name: &str) -> Option<String> {
        self.convars.borrow().get(name).map(str::to_owned)
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

    /// Removes and returns the next queued console line.
    pub(crate) fn pop_command(&self) -> Option<String> {
        self.queued.borrow_mut().pop_front()
    }

    /// Drops every queued console line and returns how many there were.
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
        let snapshot = self.snapshot();
        let ctx = EventContext::new();
        for runtime in snapshot.iter() {
            if runtime.handles_event(event) {
                if let Err(err) = runtime.deliver_event(event, source, args, &ctx) {
                    error!(target: "sixm::script", "[{}] {event}: {err}", runtime.resource());
                }
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

        for file in &files {
            if let Err(err) = runtime.load_script(file) {
                error!("[{name}] cannot load {}: {err}", file.display());
            }
        }

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
        let (files, exports) = match catalog.get(name) {
            Some(info) => (
                info.client_files(|dependency| {
                    catalog.get(dependency).map(|other| other.root.clone())
                }),
                info.manifest.exports.clone(),
            ),
            None => (Ok(Vec::new()), Vec::new()),
        };
        let files = files.unwrap_or_else(|err| {
            error!("[{name}] cannot expand its client files, none are sent: {err}");
            Vec::new()
        });
        let exports = advertisable_exports(name, exports);
        let mut hashes = self.hashes.borrow_mut();
        PublishedResource {
            name: name.to_string(),
            files: files
                .into_iter()
                .filter_map(|file| hashes.publish(file))
                .collect(),
            exports,
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
        self.dispatch_local("onResourceStop", &name);
        self.dispatch_local("onServerResourceStop", &name);
        self.release_commands_of(&name);
        self.running
            .borrow_mut()
            .retain(|candidate| !Rc::ptr_eq(candidate, runtime));
        self.rebuild_snapshot();
        self.withdraw(&name);
        runtime.shutdown();
        info!("Stopping resource {name}");
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
    ) -> Result<bool, ScriptError> {
        Ok(self.dispatch(event, source, args))
    }

    fn call_export(
        &self,
        resource: &str,
        export: &str,
        args: &[Value],
    ) -> Result<Vec<Value>, ScriptError> {
        let runtime = self
            .running_runtime(resource)
            .ok_or_else(|| ScriptError::ResourceNotRunning(resource.to_owned()))?;
        runtime.call_export(export, args)
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

    fn release_ref(&self, func: &FuncRef) {
        if is_host(func) {
            // Host functions are removed by the server, not released by holders.
            return;
        }
        if let Some(runtime) = self.runtime_of(func) {
            runtime.release_ref(func.id);
        }
    }

    fn execute_command(&self, line: &str) {
        self.queued.borrow_mut().push_back(line.to_owned());
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
        if let Some(parent) = full.parent() {
            if std::fs::create_dir_all(parent).is_err() {
                return false;
            }
        }
        std::fs::write(full, data).is_ok()
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
}

impl ServerHost for ResourceManager {
    fn trigger_client_event(
        &self,
        event: &str,
        target: ClientTarget,
        args: &[Value],
    ) -> Result<(), ScriptError> {
        self.send_client_event(event, target, args)
    }

    fn players(&self) -> Vec<u32> {
        self.player_ids()
    }

    fn player_info(&self, player: u32) -> Option<PlayerInfo> {
        self.player_details(player)
    }

    fn drop_player(&self, player: u32, reason: &str) {
        self.queue_drop(player, reason);
    }
}
