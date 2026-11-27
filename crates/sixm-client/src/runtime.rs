//! The client's script host: one [`LuaRuntime`] per synced resource, started in
//! the server's order, with the connection, the state mirror, the console and
//! the frame loop around them.

use std::cell::{Cell, RefCell};
use std::collections::{BTreeMap, HashMap, VecDeque};
use std::fs;
use std::io;
use std::path::{Component, Path, PathBuf};
use std::rc::{Rc, Weak};
use std::sync::Arc;
use std::sync::mpsc::{Receiver, RecvTimeoutError};
use std::thread;
use std::time::{Duration, Instant};

use sixm_core::{
    EntityInfo, EventContext, EventSource, FuncRef, Host, ResourceName, ScriptRuntime, Value,
};
use sixm_lua::LuaRuntime;
use sixm_resources::ClientFileKind;
use tracing::{error, info};

use crate::client::{Client, ClientNotice, SyncedResource};
use crate::convars::Convars;
use crate::state::StateMirror;

/// Input for the client loop.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Input {
    /// A console line, read from standard input.
    Line(String),
    /// Stop the client. The text is shown in the log.
    Quit(String),
}

/// A console command registered by a script with `RegisterCommand`.
#[derive(Debug, Clone)]
pub(crate) struct ScriptCommand {
    /// The function to call. The runtime holds one retain on it.
    pub(crate) handler: FuncRef,
    /// Shown by `help` as restricted. The console is the only input today.
    pub(crate) restricted: bool,
}

/// The client side of SixM: a connection to a server and the script runtimes
/// of the resources it synced.
///
/// Each synced resource runs in its own [`LuaRuntime`], in the order the server
/// started them. Scripts talk to the server through events and state bags, as
/// they do on the server. Everything runs on one thread, and no borrow of the
/// runtime's cells is held while script code runs, so scripts can call back
/// into it.
#[derive(Debug)]
pub struct ClientRuntime {
    /// Handed to every runtime as its host.
    pub(crate) me: Weak<Self>,
    /// The connection and the cache. Borrowed only to poll or send, never
    /// across a script call.
    pub(crate) client: RefCell<Client>,
    /// The server id of this client, fixed for the whole session.
    pub(crate) local_player: u32,
    /// Parent of the data folder of each resource.
    data_dir: PathBuf,
    pub(crate) convars: RefCell<Convars>,
    /// Running resources in start order.
    running: RefCell<Vec<Rc<LuaRuntime>>>,
    /// The running runtimes as one shared slice, rebuilt when `running` changes.
    /// Event fan-out and frames clone this `Rc` instead of allocating.
    snapshot: RefCell<Rc<[Rc<LuaRuntime>]>>,
    next_instance: Cell<u64>,
    /// Script commands by lower-case name.
    pub(crate) commands: RefCell<BTreeMap<String, ScriptCommand>>,
    /// Console lines queued by `ExecuteCommand`, run after the current call.
    pub(crate) queued: RefCell<VecDeque<String>>,
    /// Replicated state and local writes.
    pub(crate) state: RefCell<StateMirror>,
    /// The entities the server has created and not yet deleted, by id.
    pub(crate) entities: RefCell<BTreeMap<u32, EntityInfo>>,
    /// The synced files of each running resource: file name (as the manifest
    /// names it) to its path in the cache.
    pub(crate) files: RefCell<HashMap<String, HashMap<String, PathBuf>>>,
    pub(crate) quit: Cell<bool>,
}

impl ClientRuntime {
    /// Creates the runtime for the connection `client`. Each resource keeps its
    /// data in its own folder under `data_dir`, which is created on demand.
    /// Nothing runs until [`start_synced`](Self::start_synced).
    pub fn new(client: Client, data_dir: impl Into<PathBuf>) -> Rc<Self> {
        let local_player = client.player();
        Rc::new_cyclic(|me| Self {
            me: me.clone(),
            client: RefCell::new(client),
            local_player,
            data_dir: data_dir.into(),
            convars: RefCell::new(Convars::new()),
            running: RefCell::default(),
            snapshot: RefCell::new(Rc::from(Vec::new())),
            next_instance: Cell::new(1),
            commands: RefCell::default(),
            queued: RefCell::default(),
            state: RefCell::default(),
            entities: RefCell::default(),
            files: RefCell::default(),
            quit: Cell::new(false),
        })
    }

    /// Starts every resource the connection has synced, in the server's order.
    /// Call it once, before the first [`pump`](Self::pump) or [`run`](Self::run).
    pub fn start_synced(&self) {
        let resources = self.client.borrow().resources().to_vec();
        for resource in &resources {
            self.start_resource(resource);
        }
    }

    /// Handles the network notices that arrive within `timeout`, then every
    /// notice already waiting. Pass [`Duration::ZERO`] to only drain what is
    /// waiting, without blocking.
    pub fn pump(&self, timeout: Duration) {
        let mut wait = timeout;
        loop {
            let notice = self.client.borrow_mut().poll(wait);
            let Some(notice) = notice else {
                return;
            };
            wait = Duration::ZERO;
            self.handle_notice(notice);
        }
    }

    /// Runs one frame at `now`: resumes every resource, then runs the queued commands.
    pub fn tick(&self, now: Instant) {
        for runtime in self.snapshot().iter() {
            runtime.tick(now);
        }
        self.drain_commands();
    }

    /// Processes one input: a console line or a quit request. The commands
    /// it queued run before it returns.
    pub fn handle(&self, input: Input) {
        match input {
            Input::Line(line) => self.execute(&line),
            Input::Quit(reason) => {
                info!("Shutting down: {reason}");
                self.quit.set(true);
            }
        }
    }

    /// Runs the client until a quit is requested or the server disconnects it.
    ///
    /// Each iteration handles the network without blocking, runs a frame when
    /// one is due, runs the timers that come due between frames, and then waits
    /// for console input until the next frame or timer. Every resource is
    /// stopped on the way out.
    pub fn run(&self, input: &Receiver<Input>) {
        let mut next_frame = Instant::now();
        let mut input_open = true;
        while !self.quit.get() {
            self.pump(Duration::ZERO);
            let now = Instant::now();
            if now >= next_frame {
                next_frame = advance(next_frame, now, frame_period(self.tick_rate()));
                self.tick(now);
            } else if self.next_deadline().is_some_and(|deadline| deadline <= now) {
                self.run_due_timers(now);
            }
            if self.quit.get() {
                break;
            }

            let wake = self
                .next_deadline()
                .map_or(next_frame, |deadline| deadline.min(next_frame));
            let wait = wake.saturating_duration_since(Instant::now());
            if !input_open {
                thread::sleep(wait);
                continue;
            }
            match input.recv_timeout(wait) {
                Ok(message) => self.handle(message),
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => input_open = false,
            }
        }
        self.shutdown();
    }

    /// Stops every running resource, last started first, so each stop handler
    /// runs while its resource is still up.
    pub fn shutdown(&self) {
        self.stop_all();
        info!("Client stopped");
    }

    /// Frames per second, from the convar. See [`Convars::tick_rate`].
    fn tick_rate(&self) -> u64 {
        self.convars.borrow().tick_rate()
    }

    /// The earliest timer deadline across running resources.
    fn next_deadline(&self) -> Option<Instant> {
        self.snapshot()
            .iter()
            .filter_map(|runtime| runtime.next_deadline())
            .min()
    }

    /// Runs the due timers of every running resource, without a frame.
    fn run_due_timers(&self, now: Instant) {
        for runtime in self.snapshot().iter() {
            runtime.run_timers(now);
        }
        self.drain_commands();
    }

    /// Applies one notice from the connection.
    fn handle_notice(&self, notice: ClientNotice) {
        match notice {
            ClientNotice::ResourceStarted {
                name,
                downloaded,
                cached,
            } => {
                info!("resource {name} started: {downloaded} downloaded, {cached} cached");
                if let Some(resource) = self.synced(&name) {
                    self.start_resource(&resource);
                }
            }
            ClientNotice::ResourceFailed { name, reason } => {
                error!("resource {name} could not be synced: {reason}");
            }
            ClientNotice::ResourceStopped { name } => self.stop_named(&name),
            ClientNotice::Event { name, args } => {
                self.dispatch(&name, EventSource::Server, &args);
            }
            ClientNotice::StateBag { bag, key, value } => {
                self.state.borrow_mut().apply(&bag, &key, &value);
                self.deliver_state(&bag, &key, &value, true);
            }
            ClientNotice::StateBagRemoved { bag } => self.state.borrow_mut().remove_bag(&bag),
            ClientNotice::EntityCreated { id, info } => {
                self.entities.borrow_mut().insert(id, info);
            }
            ClientNotice::EntityTransforms { updates } => {
                let mut entities = self.entities.borrow_mut();
                for (id, position, rotation) in updates {
                    if let Some(info) = entities.get_mut(&id) {
                        info.position = position;
                        info.rotation = rotation;
                    }
                }
            }
            ClientNotice::EntityDeleted { id } => {
                self.entities.borrow_mut().remove(&id);
            }
            ClientNotice::Kicked { reason } => {
                info!("kicked: {reason}");
                self.close();
            }
            ClientNotice::Disconnected { reason } => {
                info!("disconnected: {reason}");
                self.close();
            }
        }
    }

    /// Ends the session: the connection is gone, so every resource stops.
    fn close(&self) {
        self.stop_all();
        self.quit.set(true);
    }

    /// The synced resource called `name`, copied so that no borrow of the
    /// connection is kept.
    fn synced(&self, name: &str) -> Option<SyncedResource> {
        self.client
            .borrow()
            .resources()
            .iter()
            .find(|resource| resource.name == name)
            .cloned()
    }

    /// Creates the runtime for `resource`, loads its shared and client scripts
    /// in order, then fires the start events. A resource that is already running
    /// is stopped first. A script that fails to load is logged, and the rest
    /// still load.
    fn start_resource(&self, resource: &SyncedResource) {
        if self.is_running(&resource.name) {
            self.stop_named(&resource.name);
        }
        let name: ResourceName = Arc::from(resource.name.as_str());
        let root = match self.resource_dir(&resource.name) {
            Ok(root) => root,
            Err(err) => {
                error!("[{name}] cannot prepare its data folder: {err}");
                return;
            }
        };
        let instance = self.next_instance.get();
        self.next_instance.set(instance + 1);
        let runtime = match LuaRuntime::new(
            name.clone(),
            instance,
            root,
            resource.exports.clone(),
            self.me.clone(),
        ) {
            Ok(runtime) => runtime,
            Err(err) => {
                error!("[{name}] cannot create the runtime: {err}");
                return;
            }
        };

        self.running.borrow_mut().push(Rc::clone(&runtime));
        self.rebuild_snapshot();
        let synced: HashMap<String, PathBuf> = resource
            .files
            .iter()
            .map(|file| (file.name.clone(), file.path.clone()))
            .collect();
        self.files
            .borrow_mut()
            .insert(resource.name.clone(), synced);
        for file in resource
            .files
            .iter()
            .filter(|file| file.kind != ClientFileKind::File)
        {
            if let Err(err) = runtime.load_script(&file.path) {
                error!("[{name}] cannot load {}: {err}", file.name);
            }
        }

        self.dispatch_local("onClientResourceStart", &name);
        self.dispatch_local("onResourceStart", &name);
        info!("Started resource {name}");
    }

    /// Stops the runtime of `name`, if it is running.
    fn stop_named(&self, name: &str) {
        if let Some(runtime) = self.running_runtime(name) {
            self.stop_runtime(&runtime);
        }
    }

    /// Stops every running resource, last started first.
    fn stop_all(&self) {
        loop {
            let last = self.running.borrow().last().cloned();
            let Some(runtime) = last else {
                return;
            };
            self.stop_runtime(&runtime);
        }
    }

    /// Fires the stop events, removes the runtime and its commands, and shuts it down.
    fn stop_runtime(&self, runtime: &Rc<LuaRuntime>) {
        let name = runtime.resource().clone();
        self.dispatch_local("onClientResourceStop", &name);
        self.dispatch_local("onResourceStop", &name);
        self.release_commands_of(&name);
        self.files.borrow_mut().remove(&*name);
        self.running
            .borrow_mut()
            .retain(|candidate| !Rc::ptr_eq(candidate, runtime));
        self.rebuild_snapshot();
        runtime.shutdown();
        info!("Stopping resource {name}");
    }

    /// Removes the script commands that `owner` registered, releasing their
    /// handlers while `owner` is still running.
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

    /// The data folder of `name`, created if needed.
    fn resource_dir(&self, name: &str) -> io::Result<PathBuf> {
        let dir = self.data_path(name).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("`{name}` is not a plain folder name"),
            )
        })?;
        fs::create_dir_all(&dir)?;
        Ok(dir)
    }

    /// The data folder of `resource`, without creating it. `None` unless
    /// `resource` is one plain folder name, so that a server cannot make the
    /// client reach outside its data folder.
    pub(crate) fn data_path(&self, resource: &str) -> Option<PathBuf> {
        let mut parts = Path::new(resource).components();
        match (parts.next(), parts.next()) {
            (Some(Component::Normal(_)), None) => Some(self.data_dir.join(resource)),
            _ => None,
        }
    }

    /// Whether a runtime for `name` is running.
    pub(crate) fn is_running(&self, name: &str) -> bool {
        self.running_runtime(name).is_some()
    }

    /// The running runtime called `name`.
    pub(crate) fn running_runtime(&self, name: &str) -> Option<Rc<LuaRuntime>> {
        self.running
            .borrow()
            .iter()
            .find(|runtime| runtime.resource().as_ref() == name)
            .cloned()
    }

    /// The running runtime that owns `func`, if this exact instance is still up.
    pub(crate) fn runtime_of(&self, func: &FuncRef) -> Option<Rc<LuaRuntime>> {
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
    fn dispatch_local(&self, event: &str, resource: &str) {
        self.dispatch(event, EventSource::Local, &[Value::from(resource)]);
    }

    /// Runs the state change handlers of every runtime for one write.
    pub(crate) fn deliver_state(&self, bag: &str, key: &str, value: &Value, replicated: bool) {
        for runtime in self.snapshot().iter() {
            runtime.deliver_state_change(bag, key, value, replicated);
        }
    }
}

/// The frame after `scheduled`. A client more than one frame behind schedules
/// the next frame relative to `now` instead.
fn advance(scheduled: Instant, now: Instant, period: Duration) -> Instant {
    let next = scheduled + period;
    if next > now { next } else { now + period }
}

/// Time between frames at `rate` frames per second. `rate` is at least 1.
fn frame_period(rate: u64) -> Duration {
    Duration::from_micros(1_000_000 / rate)
}
