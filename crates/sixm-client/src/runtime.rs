//! The client's script host: one runtime per script language per synced
//! resource, started in the server's order, with the connection, the state
//! mirror, the console and the frame loop around them.

use std::cell::{Cell, RefCell};
use std::collections::{BTreeMap, BTreeSet, HashMap, VecDeque};
use std::fmt;
use std::fs;
use std::io;
use std::path::{Component, Path, PathBuf};
use std::rc::{Rc, Weak};
use std::sync::Arc;
use std::sync::mpsc::{Receiver, RecvTimeoutError};
use std::thread;
use std::time::{Duration, Instant};

use sixm_core::{
    AdapterError, AdapterSink, EntityAttachment, EntityInfo, EntityKind, EntityState, EventContext,
    EventSource, FuncRef, GameAdapter, Host, MirrorChange, NativeRegistry, ResourceName,
    ResourceState, ScriptRuntime, Value, is_finite,
};
use sixm_js::JsRuntime;
use sixm_kvp::KvpStore;
use sixm_lua::LuaRuntime;
use sixm_net::{EntityStateUpdate, EntityTransform};
use sixm_resources::ClientFileKind;
use tracing::{error, info, warn};

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

/// An entity as this client keeps it: the server's last state, the teleport
/// epoch of that state, and which parts this client has moved since the last
/// frame and must tell the server.
#[derive(Debug, Clone, Copy)]
pub(crate) struct ClientEntity {
    pub(crate) info: EntityInfo,
    pub(crate) state: EntityState,
    /// The latest epoch the server sent for the entity. Owner updates carry it.
    pub(crate) epoch: u32,
    /// The position was moved since the last frame. Only an owned entity is
    /// ever queued.
    pub(crate) position_dirty: bool,
    /// The rotation was moved since the last frame.
    pub(crate) rotation_dirty: bool,
    /// The client created the entity and the server has not confirmed it yet.
    /// Its moves wait until it is confirmed, since the server does not know it.
    pub(crate) pending: bool,
}

impl ClientEntity {
    /// Whether a part was moved since the last frame, so the entity is queued.
    pub(crate) fn is_dirty(&self) -> bool {
        self.position_dirty || self.rotation_dirty
    }

    /// Takes the parts moved since the last frame as an update for the server,
    /// clearing their flags.
    fn take_update(&mut self, id: u32) -> EntityTransform {
        let position = self.position_dirty.then_some(self.info.position);
        let rotation = self.rotation_dirty.then_some(self.info.rotation);
        self.position_dirty = false;
        self.rotation_dirty = false;
        (id, position, rotation, self.epoch)
    }
}

/// A console command registered by a script with `RegisterCommand`.
#[derive(Debug, Clone)]
pub(crate) struct ScriptCommand {
    /// The function to call. The runtime holds one retain on it.
    pub(crate) handler: FuncRef,
    /// Shown by `help` as restricted. The console is the only input today.
    pub(crate) restricted: bool,
}

/// Buffered `(entity_id, position, rotation)` collected from an adapter's `read_owned`.
type OwnedTransform = (u32, [f32; 3], [f32; 3]);

/// The script runtimes of one running resource, in load order.
#[derive(Debug)]
struct RunningResource {
    name: ResourceName,
    runtimes: Vec<Rc<dyn ScriptRuntime>>,
}

/// The client side of SixM: a connection to a server and the script runtimes
/// of the resources it synced.
///
/// Each synced resource runs in its own Lua and/or JS runtime, in the order the
/// server started them. Scripts talk to the server through events and state bags,
/// as they do on the server. Everything runs on one thread, and no borrow of the
/// runtime's cells is held while script code runs, so scripts can call back
/// into it.
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
    /// The key-value stores of the resources, one `<resource>.kvp` file each in `data_dir`.
    pub(crate) kvp: RefCell<KvpStore>,
    pub(crate) convars: RefCell<Convars>,
    /// Running resources in start order.
    running: RefCell<Vec<RunningResource>>,
    /// The running runtimes as one shared slice, rebuilt when `running` changes.
    /// Event fan-out and frames clone this `Rc` instead of allocating.
    snapshot: RefCell<Rc<[Rc<dyn ScriptRuntime>]>>,
    next_instance: Cell<u64>,
    /// Script commands by lower-case name.
    pub(crate) commands: RefCell<BTreeMap<String, ScriptCommand>>,
    /// Console lines queued by `ExecuteCommand`, run after the current call.
    pub(crate) queued: RefCell<VecDeque<String>>,
    /// Replicated state and local writes.
    pub(crate) state: RefCell<StateMirror>,
    /// The entities the server has created and not yet deleted, by id.
    pub(crate) entities: RefCell<BTreeMap<u32, ClientEntity>>,
    /// The id blocks the server leased to this client, in the order they are
    /// used, each `(next, end)` as in the server.
    pub(crate) leases: RefCell<VecDeque<(u32, u32)>>,
    /// The entities this client created that still exist, confirmed or not.
    /// Their number is the client's own limit, `sixm_maxClientEntities`.
    pub(crate) own_created: RefCell<BTreeSet<u32>>,
    /// The local player's ped, once the server has created it.
    pub(crate) local_ped: Cell<Option<u32>>,
    /// Owned entities this client moved since the last frame, each listed
    /// once per move. Reused between frames.
    pub(crate) owned_moves: RefCell<Vec<u32>>,
    /// The transforms sent at the end of a frame. Reused between frames.
    owned_updates: RefCell<Vec<EntityTransform>>,
    /// The synced files of each running resource: file name (as the manifest
    /// names it) to its path in the cache.
    pub(crate) files: RefCell<HashMap<String, HashMap<String, PathBuf>>>,
    /// The metadata pairs of each running resource, as the server sent them.
    metadata: RefCell<HashMap<String, Vec<(String, String)>>>,
    /// Lifecycle states that override the running state while a resource
    /// starts or stops, oldest first.
    transitions: RefCell<Vec<(ResourceName, ResourceState)>>,
    /// Optional game adapter driving physical/simulated entities on ticks.
    adapter: RefCell<Option<Box<dyn GameAdapter>>>,
    /// Natives registered by the attached adapter, if any.
    pub(crate) adapter_natives: RefCell<Option<NativeRegistry>>,
    /// Whether an adapter is currently attached (fast guard for mirror pushes).
    has_adapter: Cell<bool>,
    /// Queued mirror changes waiting to reach the attached adapter.
    mirror_changes: RefCell<Vec<MirrorChange>>,
    /// Reused buffer collecting per-entity errors from `apply_changes`.
    adapter_errors: RefCell<Vec<(u32, AdapterError)>>,
    /// Reused buffer of owned entity ids passed to `read_owned`.
    owned_ids: RefCell<Vec<u32>>,
    /// Reused buffer of transforms returned from `read_owned`.
    read_owned_buf: RefCell<Vec<OwnedTransform>>,
    pub(crate) quit: Cell<bool>,
}

impl fmt::Debug for ClientRuntime {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ClientRuntime")
            .field("local_player", &self.local_player)
            .field("has_adapter", &self.has_adapter.get())
            .finish_non_exhaustive()
    }
}

impl ClientRuntime {
    /// Creates the runtime for the connection `client`. Each resource keeps its
    /// data in its own folder under `data_dir`, which is created on demand.
    /// Nothing runs until [`start_synced`](Self::start_synced).
    pub fn new(client: Client, data_dir: impl Into<PathBuf>) -> Rc<Self> {
        let local_player = client.player();
        let data_dir: PathBuf = data_dir.into();
        Rc::new_cyclic(|me| Self {
            me: me.clone(),
            client: RefCell::new(client),
            local_player,
            kvp: RefCell::new(KvpStore::new(data_dir.clone())),
            data_dir,
            convars: RefCell::new(Convars::new()),
            running: RefCell::default(),
            snapshot: RefCell::new(Rc::from(Vec::new())),
            next_instance: Cell::new(1),
            commands: RefCell::default(),
            queued: RefCell::default(),
            state: RefCell::default(),
            entities: RefCell::default(),
            leases: RefCell::default(),
            own_created: RefCell::default(),
            local_ped: Cell::default(),
            owned_moves: RefCell::default(),
            owned_updates: RefCell::default(),
            files: RefCell::default(),
            metadata: RefCell::default(),
            transitions: RefCell::default(),
            adapter: RefCell::default(),
            adapter_natives: RefCell::default(),
            has_adapter: Cell::new(false),
            mirror_changes: RefCell::default(),
            adapter_errors: RefCell::default(),
            owned_ids: RefCell::default(),
            read_owned_buf: RefCell::default(),
            quit: Cell::new(false),
        })
    }

    /// Installs an adapter to attach during [`start_synced`](Self::start_synced)
    /// / [`try_start_synced`](Self::try_start_synced). Must be called before
    /// starting synced resources so every runtime receives its natives.
    pub fn set_adapter(&self, adapter: Box<dyn GameAdapter>) {
        *self.adapter.borrow_mut() = Some(adapter);
    }

    /// Applies the initial snapshot, attaches any configured [`GameAdapter`],
    /// and starts every synced resource in server order.
    pub fn try_start_synced(&self) -> Result<(), AdapterError> {
        let joined = self.client.borrow_mut().join_notices();
        for notice in joined {
            self.handle_notice(notice);
        }
        self.attach_pending_adapter()?;
        let resources = self.client.borrow().resources().to_vec();
        for resource in &resources {
            self.start_resource(resource);
        }
        Ok(())
    }

    /// Starts every resource the connection has synced, in the server's order.
    /// Call it once, before the first [`pump`](Self::pump) or [`run`](Self::run).
    pub fn start_synced(&self) {
        if let Err(err) = self.try_start_synced() {
            error!("cannot attach game adapter: {err}");
        }
    }

    fn attach_pending_adapter(&self) -> Result<(), AdapterError> {
        let Some(mut adapter) = self.adapter.borrow_mut().take() else {
            return Ok(());
        };
        if let Err(err) = adapter.attach(AdapterSink::new(self.me.clone())) {
            self.has_adapter.set(false);
            *self.adapter_natives.borrow_mut() = None;
            return Err(err);
        }
        *self.adapter_natives.borrow_mut() = Some(adapter.natives().clone());
        self.has_adapter.set(true);
        // Seed any entities already present from join notices into the adapter queue.
        let mut seed = Vec::new();
        for (&id, slot) in self.entities.borrow().iter() {
            seed.push(MirrorChange::Created {
                id,
                info: slot.info,
            });
            if slot.state != EntityState::default() {
                seed.push(MirrorChange::StateChanged {
                    id,
                    state: slot.state,
                });
            }
        }
        if !seed.is_empty() {
            let mut changes = self.mirror_changes.borrow_mut();
            let tail = std::mem::take(&mut *changes);
            *changes = seed;
            changes.extend(tail);
        }
        *self.adapter.borrow_mut() = Some(adapter);
        Ok(())
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

    /// Runs one frame at `now`: delivers mirror updates and runs adapter
    /// `before_scripts`, resumes resources, runs queued commands, pushes any
    /// script moves to the adapter, runs `after_scripts`, reads owned entity
    /// transforms from the adapter, and sends dirty transforms to the server.
    pub fn tick(&self, now: Instant) {
        self.flush_mirror_changes();
        self.with_adapter(|adapter| adapter.before_scripts(now));
        for runtime in self.snapshot().iter() {
            runtime.tick(now);
        }
        self.drain_commands();
        self.flush_mirror_changes();
        self.with_adapter(|adapter| adapter.after_scripts(now));
        self.collect_adapter_owned_moves();
        self.send_owned_moves();
        self.kvp.borrow_mut().flush_due(now);
    }

    pub(crate) fn push_mirror_change(&self, change: MirrorChange) {
        if self.has_adapter.get() {
            self.mirror_changes.borrow_mut().push(change);
        }
    }

    fn with_adapter(&self, f: impl FnOnce(&mut dyn GameAdapter)) {
        let Some(mut adapter) = self.adapter.borrow_mut().take() else {
            return;
        };
        f(&mut *adapter);
        *self.adapter.borrow_mut() = Some(adapter);
    }

    fn flush_mirror_changes(&self) {
        if !self.has_adapter.get() || self.mirror_changes.borrow().is_empty() {
            return;
        }
        let mut changes = std::mem::take(&mut *self.mirror_changes.borrow_mut());
        let mut errors = std::mem::take(&mut *self.adapter_errors.borrow_mut());
        errors.clear();
        self.with_adapter(|adapter| adapter.apply_changes(&changes, &mut errors));
        for (entity, err) in errors.drain(..) {
            warn!("adapter error on entity {entity}: {err}");
        }
        changes.clear();
        *self.mirror_changes.borrow_mut() = changes;
        *self.adapter_errors.borrow_mut() = errors;
    }

    fn collect_adapter_owned_moves(&self) {
        if !self.has_adapter.get() {
            return;
        }
        let mut owned = std::mem::take(&mut *self.owned_ids.borrow_mut());
        owned.clear();
        for (&id, slot) in self.entities.borrow().iter() {
            if slot.info.owner == Some(self.local_player)
                && !slot.state.frozen
                && slot.state.attachment.is_none()
            {
                owned.push(id);
            }
        }
        if !owned.is_empty() {
            let mut out = std::mem::take(&mut *self.read_owned_buf.borrow_mut());
            out.clear();
            self.with_adapter(|adapter| adapter.read_owned(&owned, &mut out));
            for (id, position, rotation) in out.drain(..) {
                self.apply_adapter_owned_move(id, position, rotation);
            }
            *self.read_owned_buf.borrow_mut() = out;
        }
        *self.owned_ids.borrow_mut() = owned;
    }

    fn apply_adapter_owned_move(&self, entity: u32, position: [f32; 3], rotation: [f32; 3]) {
        if !is_finite(position) || !is_finite(rotation) {
            warn!("adapter returned non-finite transform for entity {entity}");
            return;
        }
        let mut entities = self.entities.borrow_mut();
        let Some(slot) = entities.get_mut(&entity) else {
            return;
        };
        if slot.info.owner != Some(self.local_player)
            || slot.state.frozen
            || slot.state.attachment.is_some()
        {
            return;
        }
        let queued = slot.is_dirty();
        let mut changed = false;
        if vec_bits_differ(slot.info.position, position) {
            slot.info.position = position;
            slot.position_dirty = true;
            changed = true;
        }
        if vec_bits_differ(slot.info.rotation, rotation) {
            slot.info.rotation = rotation;
            slot.rotation_dirty = true;
            changed = true;
        }
        if changed && !queued && slot.is_dirty() {
            self.owned_moves.borrow_mut().push(entity);
        }
    }

    fn detach_adapter(&self) {
        self.has_adapter.set(false);
        *self.adapter_natives.borrow_mut() = None;
        self.mirror_changes.borrow_mut().clear();
        if let Some(mut adapter) = self.adapter.borrow_mut().take() {
            adapter.detach();
        }
    }

    /// Sends the latest transform of each owned entity moved since the last
    /// frame. Nothing is sent when nothing moved. The buffers are reused, so a
    /// frame allocates only when the connection queues the message.
    fn send_owned_moves(&self) {
        if self.owned_moves.borrow().is_empty() {
            return;
        }
        let mut updates = std::mem::take(&mut *self.owned_updates.borrow_mut());
        let mut waiting = Vec::new();
        {
            let mut moved = self.owned_moves.borrow_mut();
            let mut entities = self.entities.borrow_mut();
            for id in moved.drain(..) {
                match entities.get_mut(&id) {
                    // Not confirmed yet: the server would ignore the move, so it
                    // stays queued until the entity is confirmed.
                    Some(slot) if slot.pending => waiting.push(id),
                    // A move listed twice is sent once, because the first one clears the flags.
                    Some(slot) if slot.is_dirty() => updates.push(slot.take_update(id)),
                    _ => {}
                }
            }
        }
        self.owned_moves.borrow_mut().extend(waiting);
        if !updates.is_empty()
            && let Err(err) = self.client.borrow().send_entity_transforms(&updates)
        {
            warn!("cannot send the entity transforms: {err}");
        }
        updates.clear();
        *self.owned_updates.borrow_mut() = updates;
    }

    /// Records a created entity, and the local player's ped when it is one. An
    /// entity this client created and was waiting for is confirmed: it keeps
    /// the local transform, which is the newer one, and takes the epoch and state.
    fn create_entity(&self, id: u32, info: EntityInfo, state: EntityState, epoch: u32) {
        if info.kind == EntityKind::Ped && info.owner == Some(self.local_player) {
            self.local_ped.set(Some(id));
        }
        let confirmed_pending = {
            let mut entities = self.entities.borrow_mut();
            match entities.get_mut(&id) {
                Some(slot) if slot.pending => {
                    slot.pending = false;
                    slot.epoch = epoch;
                    slot.state = state;
                    true
                }
                _ => {
                    entities.insert(
                        id,
                        ClientEntity {
                            info,
                            state,
                            epoch,
                            position_dirty: false,
                            rotation_dirty: false,
                            pending: false,
                        },
                    );
                    false
                }
            }
        };
        if !confirmed_pending {
            self.push_mirror_change(MirrorChange::Created { id, info });
        }
        if state != EntityState::default() {
            self.push_mirror_change(MirrorChange::StateChanged { id, state });
        }
    }

    /// Forgets a deleted entity, and the local ped when it was that one.
    fn delete_entity(&self, id: u32) {
        if self.local_ped.get() == Some(id) {
            self.local_ped.set(None);
        }
        self.own_created.borrow_mut().remove(&id);
        if self.entities.borrow_mut().remove(&id).is_some() {
            self.push_mirror_change(MirrorChange::Removed { id });
        }
    }

    /// Takes the next id of the leases, and drops a block once it is used up.
    /// `None` when no id is leased yet.
    pub(crate) fn take_leased_id(&self) -> Option<u32> {
        let mut leases = self.leases.borrow_mut();
        let front = leases.front_mut()?;
        let id = front.0;
        front.0 += 1;
        if front.0 == front.1 {
            leases.pop_front();
        }
        Some(id)
    }

    /// Adds a block of ids the server leased to this client.
    fn lease_ids(&self, first: u32, count: u32) {
        if let Some(end) = first.checked_add(count) {
            self.leases.borrow_mut().push_back((first, end));
        }
    }

    /// Removes an entity the server refused, which this client had created
    /// provisionally, and logs why.
    fn refuse_entity(&self, id: u32, reason: &str) {
        warn!("the server refused entity {id}: {reason}");
        self.state
            .borrow_mut()
            .remove_bag(&sixm_core::entity_bag(id));
        self.delete_entity(id);
    }

    /// Applies a change of owner from the server. The new owner takes the given
    /// transform and starts from `epoch`, the entity's current teleport epoch,
    /// and any local move of this client that was not sent is dropped, since
    /// the server's transform is the one that counts now.
    fn apply_owner(&self, id: u32, owner: u32, epoch: u32, position: [f32; 3], rotation: [f32; 3]) {
        let updated = {
            let mut entities = self.entities.borrow_mut();
            let Some(slot) = entities.get_mut(&id) else {
                return;
            };
            slot.info.owner = (owner != 0).then_some(owner);
            slot.info.position = position;
            slot.info.rotation = rotation;
            slot.epoch = epoch;
            slot.position_dirty = false;
            slot.rotation_dirty = false;
            slot.info
        };
        self.push_mirror_change(MirrorChange::OwnerChanged { id, info: updated });
    }

    /// Applies the transforms the server sent. An update with the epoch the
    /// client already knows for an entity it owns is its own move echoed back,
    /// and is skipped. Any other update is a server move: each part it carries
    /// replaces the client's part and drops a local move of that part not yet
    /// sent, and the new epoch is taken. A part it does not carry keeps the
    /// client's value, and a local move of it is still sent. Unknown entities
    /// are ignored.
    fn apply_transforms(&self, updates: &[EntityTransform]) {
        if self.has_adapter.get() {
            let mut entities = self.entities.borrow_mut();
            let mut changes = self.mirror_changes.borrow_mut();
            for &(id, position, rotation, epoch) in updates {
                let Some(slot) = entities.get_mut(&id) else {
                    continue;
                };
                if slot.info.owner == Some(self.local_player) && slot.epoch == epoch {
                    continue;
                }
                let teleport = slot.epoch != epoch;
                if let Some(position) = position {
                    slot.info.position = position;
                    slot.position_dirty = false;
                }
                if let Some(rotation) = rotation {
                    slot.info.rotation = rotation;
                    slot.rotation_dirty = false;
                }
                slot.epoch = epoch;
                changes.push(MirrorChange::Moved {
                    id,
                    info: slot.info,
                    teleport,
                });
            }
            return;
        }
        let mut entities = self.entities.borrow_mut();
        for &(id, position, rotation, epoch) in updates {
            let Some(slot) = entities.get_mut(&id) else {
                continue;
            };
            if slot.info.owner == Some(self.local_player) && slot.epoch == epoch {
                continue;
            }
            if let Some(position) = position {
                slot.info.position = position;
                slot.position_dirty = false;
            }
            if let Some(rotation) = rotation {
                slot.info.rotation = rotation;
                slot.rotation_dirty = false;
            }
            slot.epoch = epoch;
        }
    }

    /// Applies delta entity state updates received from the server. Unknown
    /// entities are ignored.
    fn apply_states(&self, updates: &[EntityStateUpdate]) {
        let mut entities = self.entities.borrow_mut();
        let has_adapter = self.has_adapter.get();
        let mut changes = has_adapter.then(|| self.mirror_changes.borrow_mut());
        for update in updates {
            let Some(slot) = entities.get_mut(&update.id) else {
                continue;
            };
            if let Some(frozen) = update.frozen {
                slot.state.frozen = frozen;
                if frozen {
                    slot.position_dirty = false;
                    slot.rotation_dirty = false;
                }
            }
            if let Some(velocity) = update.velocity {
                slot.state.velocity = velocity;
            }
            if let Some(health) = update.health {
                slot.state.health = health;
            }
            if let Some(max_health) = update.max_health {
                slot.state.max_health = max_health;
            }
            if let Some(visible) = update.visible {
                slot.state.visible = visible;
            }
            if let Some(alpha) = update.alpha {
                slot.state.alpha = alpha;
            }
            if let Some(attachment) = update.attachment {
                slot.state.attachment = attachment.map(EntityAttachment::from);
                if slot.state.attachment.is_some() {
                    slot.position_dirty = false;
                    slot.rotation_dirty = false;
                }
            }
            if let Some(ref mut changes) = changes {
                changes.push(MirrorChange::StateChanged {
                    id: update.id,
                    state: slot.state,
                });
            }
        }
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
        self.detach_adapter();
        self.kvp.borrow_mut().shutdown();
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
            ClientNotice::EntityCreated {
                id,
                info,
                state,
                epoch,
            } => {
                self.create_entity(id, info, state, epoch);
            }
            ClientNotice::EntityTransforms { updates } => self.apply_transforms(&updates),
            ClientNotice::EntityStates { updates } => self.apply_states(&updates),
            ClientNotice::EntityDeleted { id } => self.delete_entity(id),
            ClientNotice::EntityIdLease { first, count } => self.lease_ids(first, count),
            ClientNotice::EntityCreateRefused { id, reason } => self.refuse_entity(id, &reason),
            ClientNotice::EntityOwner {
                id,
                owner,
                epoch,
                position,
                rotation,
            } => self.apply_owner(id, owner, epoch, position, rotation),
            ClientNotice::Convars { entries } => {
                self.convars.borrow_mut().apply_replicated(entries);
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
        self.detach_adapter();
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

    /// Creates the runtime(s) for `resource`, loads its shared and client scripts
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
        let scripts: Vec<_> = resource
            .files
            .iter()
            .filter(|file| file.kind != ClientFileKind::File)
            .collect();

        let (runtimes, lua_rt, js_rt) = match self.create_runtimes(
            &name,
            &root,
            &resource.exports,
            scripts.iter().map(|file| Path::new(&file.name)),
        ) {
            Ok(created) => created,
            Err(err) => {
                error!("[{name}] cannot create the runtime: {err}");
                return;
            }
        };

        self.running.borrow_mut().push(RunningResource {
            name: name.clone(),
            runtimes,
        });
        self.rebuild_snapshot();
        let synced: HashMap<String, PathBuf> = resource
            .files
            .iter()
            .map(|file| (file.name.clone(), file.path.clone()))
            .collect();
        self.files
            .borrow_mut()
            .insert(resource.name.clone(), synced);
        self.metadata
            .borrow_mut()
            .insert(resource.name.clone(), resource.metadata.clone());
        self.set_transition(&name, ResourceState::Starting);
        for file in scripts {
            // Check `file.name` (the cache stores files under extensionless content hashes).
            let result = if is_js_script(Path::new(&file.name)) {
                js_rt.as_ref().map(|rt| rt.load_script(&file.path))
            } else {
                lua_rt.as_ref().map(|rt| rt.load_script(&file.path))
            };
            if let Some(Err(err)) = result {
                error!("[{name}] cannot load {}: {err}", file.name);
            }
        }
        self.clear_transition(&name);

        self.dispatch_local("onClientResourceStart", &name);
        self.dispatch_local("onResourceStart", &name);
        info!("Started resource {name}");
    }

    /// Creates up to one Lua runtime and one JS runtime matching the extensions
    /// present in `scripts`, in order of first appearance.
    #[expect(clippy::type_complexity)]
    fn create_runtimes<'a>(
        &self,
        name: &ResourceName,
        root: &Path,
        exports: &[String],
        scripts: impl IntoIterator<Item = &'a Path>,
    ) -> Result<
        (
            Vec<Rc<dyn ScriptRuntime>>,
            Option<Rc<LuaRuntime>>,
            Option<Rc<JsRuntime>>,
        ),
        sixm_core::ScriptError,
    > {
        let mut runtimes: Vec<Rc<dyn ScriptRuntime>> = Vec::new();
        let mut lua_rt: Option<Rc<LuaRuntime>> = None;
        let mut js_rt: Option<Rc<JsRuntime>> = None;

        for path in scripts {
            if is_js_script(path) {
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

    /// Stops the resource `name`, if it is running.
    fn stop_named(&self, name: &str) {
        let resource = self
            .running
            .borrow()
            .iter()
            .find(|res| res.name.as_ref() == name)
            .map(|res| res.name.clone());
        if let Some(name) = resource {
            self.stop_resource(&name);
        }
    }

    /// Stops every running resource, last started first.
    fn stop_all(&self) {
        loop {
            let last = self.running.borrow().last().map(|res| res.name.clone());
            let Some(name) = last else {
                return;
            };
            self.stop_resource(&name);
        }
    }

    /// Fires the stop events, removes the resource's runtimes and commands, and shuts them down.
    fn stop_resource(&self, name: &ResourceName) {
        self.set_transition(name, ResourceState::Stopping);
        self.dispatch_local("onClientResourceStop", name);
        self.dispatch_local("onResourceStop", name);
        self.release_commands_of(name);
        self.files.borrow_mut().remove(name.as_ref());
        self.metadata.borrow_mut().remove(name.as_ref());
        let removed = {
            let mut running = self.running.borrow_mut();
            running
                .iter()
                .position(|res| res.name == *name)
                .map(|index| running.remove(index))
        };
        self.rebuild_snapshot();
        self.clear_transition(name);
        if let Some(resource) = removed {
            for runtime in resource.runtimes.into_iter().rev() {
                runtime.shutdown();
            }
        }
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

    /// Whether a resource called `name` is running.
    pub(crate) fn is_running(&self, name: &str) -> bool {
        self.running
            .borrow()
            .iter()
            .any(|res| res.name.as_ref() == name)
    }

    /// The running runtimes of the resource called `name`, in load order.
    pub(crate) fn running_runtimes(&self, name: &str) -> Option<Vec<Rc<dyn ScriptRuntime>>> {
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
    fn dispatch_local(&self, event: &str, resource: &str) {
        self.dispatch(event, EventSource::Local, &[Value::from(resource)]);
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

    /// The lifecycle state of `resource`: a transition in progress, else
    /// `Started` for a running resource and `Missing` for any other.
    pub(crate) fn state_of(&self, resource: &str) -> ResourceState {
        let transitional = self
            .transitions
            .borrow()
            .iter()
            .rev()
            .find(|(owner, _)| owner.as_ref() == resource)
            .map(|(_, state)| *state);
        match transitional {
            Some(state) => state,
            None if self.is_running(resource) => ResourceState::Started,
            None => ResourceState::Missing,
        }
    }

    /// Names of the running resources, in start order.
    pub(crate) fn running_names(&self) -> Vec<ResourceName> {
        self.running
            .borrow()
            .iter()
            .map(|res| res.name.clone())
            .collect()
    }

    /// The values of metadata key `key` of the running resource `resource`, in
    /// declaration order. Empty when it is not running or has no such key.
    pub(crate) fn metadata_of(&self, resource: &str, key: &str) -> Vec<String> {
        self.metadata
            .borrow()
            .get(resource)
            .map(|pairs| {
                pairs
                    .iter()
                    .filter(|(name, _)| name == key)
                    .map(|(_, value)| value.clone())
                    .collect()
            })
            .unwrap_or_default()
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

fn is_js_script(path: &Path) -> bool {
    path.extension()
        .and_then(|ext| ext.to_str())
        .is_some_and(|ext| ext.eq_ignore_ascii_case("js"))
}

fn vec_bits_differ(a: [f32; 3], b: [f32; 3]) -> bool {
    a[0].to_bits() != b[0].to_bits()
        || a[1].to_bits() != b[1].to_bits()
        || a[2].to_bits() != b[2].to_bits()
}

/// Time between frames at `rate` frames per second. `rate` is at least 1.
fn frame_period(rate: u64) -> Duration {
    Duration::from_micros(1_000_000 / rate)
}
