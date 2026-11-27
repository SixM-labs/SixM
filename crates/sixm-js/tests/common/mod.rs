#![allow(dead_code)]
//! A minimal server for the tests: it owns the runtimes and routes events,
//! exports, function references and console commands between them.

use std::cell::{Cell, RefCell};
use std::collections::{BTreeMap, HashMap};
use std::fs;
use std::path::PathBuf;
use std::rc::{Rc, Weak};
use std::sync::Arc;
use std::time::{Duration, Instant};

use sixm_core::{
    ClientHost, ClientTarget, ConvarKind, EntityAttachment, EntityInfo, EntityKind, EntityState,
    EventContext, EventSource, FuncRef, GLOBAL_BAG, Host, HttpRequest, PlayerInfo, ResourceName,
    ResourceState, ScriptError, ScriptRuntime, ServerHost, Value, WebReply, entity_bag,
    entity_of_bag, is_finite, player_of_bag,
};
use sixm_js::JsRuntime;
use sixm_kvp::KvpStore;
use tempfile::TempDir;

/// Which side of the connection a [`TestHost`] plays.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Side {
    /// `server()` is available and `client()` is not.
    #[default]
    Server,
    /// `client()` is available and `server()` is not.
    Client,
}

/// Owns runtimes in start order and routes calls between them.
/// A move of an owned entity: the entity, and the parts of its transform that moved.
pub type OwnedMove = (u32, Option<[f32; 3]>, Option<[f32; 3]>);
pub type LatentClientEvent = (String, ClientTarget, Vec<Value>, u64);

#[derive(Debug, Default)]
pub struct TestHost {
    /// The side this host plays. Decides which services it exposes.
    pub side: Side,
    /// The client's server id, returned by `local_player` on a client host.
    pub local_player: u32,
    /// Events sent to the server by a client host, as `(event, args)`, in order.
    pub server_events: RefCell<Vec<(String, Vec<Value>)>>,
    pub latent_server_events: RefCell<Vec<(String, Vec<Value>, u64)>>,
    runtimes: RefCell<Vec<Rc<JsRuntime>>>,
    roots: RefCell<HashMap<String, PathBuf>>,
    dirs: RefCell<Vec<TempDir>>,
    convars: HashMap<String, String>,
    next_instance: Cell<u64>,
    /// Function references released by runtimes, in order.
    pub released: RefCell<Vec<FuncRef>>,
    /// Lines queued by `ExecuteCommand`.
    pub commands: RefCell<Vec<String>>,
    /// Convars stored by a server host, as `(name, value, kind)`, in order.
    pub convar_sets: RefCell<Vec<(String, String, ConvarKind)>>,
    /// Connected players as `(id, info)`, in join order. Tests set this directly.
    pub players: RefCell<Vec<(u32, PlayerInfo)>>,
    /// Identifiers of players as `(id, identifiers)`. Tests set this directly.
    pub identifiers: RefCell<Vec<(u32, Vec<String>)>>,
    /// Client events sent, as `(event, target, args)`, in order.
    pub client_events: RefCell<Vec<(String, ClientTarget, Vec<Value>)>>,
    pub latent_client_events: RefCell<Vec<LatentClientEvent>>,
    /// Players dropped, as `(id, reason, resource)`, in order.
    pub dropped: RefCell<Vec<(u32, String, String)>>,
    /// State values by `(bag, key)`.
    pub state: RefCell<HashMap<(String, String), Value>>,
    /// The key-value store, in a temporary folder unless [`TestHost::set_kvp_dir`] chose one.
    kvp: RefCell<Option<KvpStore>>,
    /// Entities by id, in id order. On a server they are created and deleted
    /// through the host; on a client a test fills them in directly.
    pub entities: RefCell<BTreeMap<u32, EntityInfo>>,
    /// Replicated entity state by entity id.
    pub entity_states: RefCell<BTreeMap<u32, EntityState>>,
    /// The ped of each player by player id, as a server host reports it.
    pub player_peds: RefCell<HashMap<u32, u32>>,
    /// Routing buckets of players and entities outside bucket 0, by id.
    pub player_buckets: RefCell<BTreeMap<u32, u32>>,
    pub entity_buckets: RefCell<BTreeMap<u32, u32>>,
    /// Population switches and lockdown modes of buckets, by bucket.
    pub population: RefCell<BTreeMap<u32, bool>>,
    pub lockdown: RefCell<BTreeMap<u32, String>>,
    /// Orphan modes set on entities by a server host, by entity.
    pub orphan_modes: RefCell<BTreeMap<u32, String>>,
    /// Entity ids a client host asked the server to create, in order.
    pub created_requests: RefCell<Vec<u32>>,
    /// Entity ids a client host asked the server to give it, in order.
    pub control_requests: RefCell<Vec<u32>>,
    /// The ped a client host reports as the local player's, once set.
    pub local_ped: Cell<Option<u32>>,
    /// Moves a client host accepted, as `(entity, position, rotation)`, in order.
    pub owned_moves: RefCell<Vec<OwnedMove>>,
    /// The last entity id handed out. Ids start at 1.
    pub last_entity: Cell<u32>,
    /// Lifecycle states set by a test. A resource without one is `Started` when
    /// it was started with [`start`] and `Missing` otherwise.
    pub states: RefCell<HashMap<String, ResourceState>>,
    /// Manifest metadata pairs by resource name, in declaration order.
    pub metadata: RefCell<HashMap<String, Vec<(String, String)>>>,
    /// HTTP requests started by scripts, as `(resource, request, callback)`.
    /// Nothing performs them: a test delivers the outcome by calling `callback`.
    pub http_requests: RefCell<Vec<(ResourceName, HttpRequest, FuncRef)>>,
    /// HTTP handlers registered by `SetHttpHandler`, as `(resource, function)`.
    pub web_handlers: RefCell<Vec<(ResourceName, FuncRef)>>,
    /// Steps of the answers that handlers gave, as `(request, step)`, in order.
    pub web_replies: RefCell<Vec<(u64, WebReply)>>,
    /// Access grants as `(principal, object)` pairs. A principal has exactly the
    /// objects it is granted: the test host does not model inheritance.
    pub grants: RefCell<Vec<(String, String)>>,
    /// Resource lifecycle calls recorded as `(action, caller, resource)`.
    pub resource_actions: RefCell<Vec<(&'static str, String, String)>>,
    /// Console commands owned by a resource, as `(resource, command)`. Tests set this directly.
    pub command_owners: RefCell<Vec<(String, String)>>,
    /// How long each player has been connected, by player id. Tests set this directly.
    pub time_online: RefCell<HashMap<u32, Duration>>,
}

impl TestHost {
    fn snapshot(&self) -> Vec<Rc<JsRuntime>> {
        self.runtimes.borrow().clone()
    }

    fn find(&self, resource: &str) -> Option<Rc<JsRuntime>> {
        self.runtimes
            .borrow()
            .iter()
            .rev()
            .find(|runtime| runtime.resource().as_ref() == resource)
            .cloned()
    }

    fn find_instance(&self, resource: &str, instance: u64) -> Option<Rc<JsRuntime>> {
        self.runtimes
            .borrow()
            .iter()
            .find(|runtime| {
                runtime.resource().as_ref() == resource && runtime.instance() == instance
            })
            .cloned()
    }

    /// Removes `runtime` from the host and shuts it down.
    pub fn stop(&self, runtime: &Rc<JsRuntime>) {
        self.runtimes
            .borrow_mut()
            .retain(|candidate| !Rc::ptr_eq(candidate, runtime));
        runtime.shutdown();
    }
}

impl Host for TestHost {
    fn server(&self) -> Option<&dyn ServerHost> {
        match self.side {
            Side::Server => Some(self),
            Side::Client => None,
        }
    }

    fn client(&self) -> Option<&dyn ClientHost> {
        match self.side {
            Side::Server => None,
            Side::Client => Some(self),
        }
    }

    fn trigger_event(
        &self,
        event: &str,
        source: EventSource,
        args: &[Value],
        invoker: Option<&ResourceName>,
    ) -> Result<bool, ScriptError> {
        let ctx = invoker.map_or_else(EventContext::new, |name| {
            EventContext::invoked_by(name.clone())
        });
        for runtime in self.snapshot() {
            if runtime.handles_event(event) {
                runtime.deliver_event(event, source, args, &ctx)?;
            }
        }
        Ok(ctx.is_canceled())
    }

    fn call_export(
        &self,
        resource: &str,
        export: &str,
        args: &[Value],
        invoker: Option<&ResourceName>,
    ) -> Result<Vec<Value>, ScriptError> {
        let runtime = self
            .find(resource)
            .ok_or_else(|| ScriptError::ResourceNotRunning(resource.to_owned()))?;
        runtime.call_export(export, args, invoker)
    }

    fn resource_state(&self, resource: &str) -> ResourceState {
        if let Some(state) = self.states.borrow().get(resource) {
            return *state;
        }
        if self.roots.borrow().contains_key(resource) {
            ResourceState::Started
        } else {
            ResourceState::Missing
        }
    }

    fn resource_names(&self) -> Vec<ResourceName> {
        let mut names: Vec<ResourceName> = self
            .roots
            .borrow()
            .keys()
            .map(|name| Arc::from(name.as_str()))
            .collect();
        names.sort();
        names
    }

    fn resource_metadata(&self, resource: &str, key: &str) -> Vec<String> {
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

    fn call_ref(&self, func: &FuncRef, args: &[Value]) -> Result<Vec<Value>, ScriptError> {
        let runtime = self
            .find_instance(&func.owner, func.instance)
            .ok_or_else(|| ScriptError::StaleRef(func.clone()))?;
        runtime.call_ref(func.id, args)
    }

    fn retain_ref(&self, func: &FuncRef) -> bool {
        self.find_instance(&func.owner, func.instance)
            .is_some_and(|runtime| runtime.retain_ref(func.id))
    }

    fn release_ref(&self, func: &FuncRef) {
        self.released.borrow_mut().push(func.clone());
        if let Some(runtime) = self.find_instance(&func.owner, func.instance) {
            runtime.release_ref(func.id);
        }
    }

    fn perform_http_request(
        &self,
        resource: &ResourceName,
        request: HttpRequest,
        callback: FuncRef,
    ) -> Result<(), ScriptError> {
        self.http_requests
            .borrow_mut()
            .push((resource.clone(), request, callback));
        Ok(())
    }

    fn execute_command(&self, line: &str, _invoker: Option<&ResourceName>) {
        self.commands.borrow_mut().push(line.to_owned());
    }

    fn register_command(&self, _name: &str, _handler: FuncRef, _restricted: bool) {}

    fn get_convar(&self, name: &str) -> Option<String> {
        self.convars.get(name).cloned()
    }

    fn resource_path(&self, resource: &str) -> Option<PathBuf> {
        self.roots.borrow().get(resource).cloned()
    }

    fn load_resource_file(&self, resource: &str, path: &str) -> Option<Vec<u8>> {
        let root = self.resource_path(resource)?;
        fs::read(root.join(path)).ok()
    }

    fn save_resource_file(&self, resource: &str, path: &str, data: &[u8]) -> bool {
        let Some(root) = self.resource_path(resource) else {
            return false;
        };
        let full = root.join(path);
        if let Some(parent) = full.parent()
            && fs::create_dir_all(parent).is_err()
        {
            return false;
        }
        fs::write(full, data).is_ok()
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
        self.state
            .borrow()
            .get(&(bag.to_owned(), key.to_owned()))
            .cloned()
    }

    fn state_keys(&self, bag: &str) -> Option<Vec<String>> {
        let player_connected = player_of_bag(bag).is_some_and(|id| {
            self.players
                .borrow()
                .iter()
                .any(|(connected, _)| *connected == id)
        });
        let entity_exists =
            entity_of_bag(bag).is_some_and(|id| self.entities.borrow().contains_key(&id));
        if bag != GLOBAL_BAG && !player_connected && !entity_exists {
            return None;
        }
        let mut keys: Vec<String> = self
            .state
            .borrow()
            .keys()
            .filter(|(owner, _)| owner.as_str() == bag)
            .map(|(_, key)| key.clone())
            .collect();
        keys.sort();
        Some(keys)
    }

    fn entity(&self, entity: u32) -> Option<EntityInfo> {
        self.entities.borrow().get(&entity).copied()
    }

    fn entity_state(&self, entity: u32) -> Option<EntityState> {
        self.entities.borrow().contains_key(&entity).then(|| {
            self.entity_states
                .borrow()
                .get(&entity)
                .copied()
                .unwrap_or_default()
        })
    }

    fn entities(&self, kind: Option<EntityKind>) -> Vec<u32> {
        self.entities
            .borrow()
            .iter()
            .filter(|(_, info)| kind.is_none_or(|wanted| info.kind == wanted))
            .map(|(id, _)| *id)
            .collect()
    }

    /// Stores the value like the server does, refusing unknown player bags, and
    /// runs the change handlers of every runtime. Replication is not modelled.
    fn state_set(
        &self,
        bag: &str,
        key: &str,
        value: Value,
        replicated: bool,
    ) -> Result<(), ScriptError> {
        let player_connected = player_of_bag(bag).is_some_and(|id| {
            self.players
                .borrow()
                .iter()
                .any(|(connected, _)| *connected == id)
        });
        let entity_exists =
            entity_of_bag(bag).is_some_and(|id| self.entities.borrow().contains_key(&id));
        if bag != GLOBAL_BAG && !player_connected && !entity_exists {
            return Err(ScriptError::Runtime(format!(
                "state bag `{bag}` does not exist"
            )));
        }
        if value.contains_func_ref() {
            return Err(ScriptError::Runtime(
                "state values cannot contain functions".to_owned(),
            ));
        }
        let slot = (bag.to_owned(), key.to_owned());
        if matches!(value, Value::Nil) {
            self.state.borrow_mut().remove(&slot);
        } else {
            self.state.borrow_mut().insert(slot, value.clone());
        }
        for runtime in self.snapshot() {
            runtime.deliver_state_change(bag, key, &value, replicated);
        }
        Ok(())
    }
    fn resource_commands(&self, resource: &str) -> Vec<String> {
        let mut names: Vec<String> = self
            .command_owners
            .borrow()
            .iter()
            .filter(|(owner, _)| owner == resource)
            .map(|(_, name)| name.clone())
            .collect();
        names.sort_unstable();
        names
    }
}

impl ServerHost for TestHost {
    fn trigger_client_event(
        &self,
        event: &str,
        target: ClientTarget,
        args: &[Value],
    ) -> Result<(), ScriptError> {
        if args.iter().any(Value::contains_func_ref) {
            return Err(ScriptError::Runtime(
                "function references cannot be sent to clients".to_owned(),
            ));
        }
        self.client_events
            .borrow_mut()
            .push((event.to_owned(), target, args.to_vec()));
        Ok(())
    }

    fn trigger_latent_client_event(
        &self,
        event: &str,
        target: ClientTarget,
        args: &[Value],
        bps: u64,
    ) -> Result<(), ScriptError> {
        self.latent_client_events
            .borrow_mut()
            .push((event.to_owned(), target, args.to_vec(), bps));
        Ok(())
    }

    fn players(&self) -> Vec<u32> {
        self.players.borrow().iter().map(|(id, _)| *id).collect()
    }

    fn player_info(&self, player: u32) -> Option<PlayerInfo> {
        self.players
            .borrow()
            .iter()
            .find(|(id, _)| *id == player)
            .map(|(_, info)| info.clone())
    }

    fn player_identifiers(&self, player: u32) -> Vec<String> {
        self.identifiers
            .borrow()
            .iter()
            .find(|(id, _)| *id == player)
            .map(|(_, identifiers)| identifiers.clone())
            .unwrap_or_default()
    }

    fn drop_player(&self, player: u32, reason: &str, resource: &str) {
        self.dropped
            .borrow_mut()
            .push((player, reason.to_owned(), resource.to_owned()));
    }

    fn player_time_online(&self, player: u32) -> Option<Duration> {
        self.time_online.borrow().get(&player).copied()
    }

    fn is_ace_allowed(&self, principal: &str, object: &str) -> bool {
        self.grants
            .borrow()
            .iter()
            .any(|(granted, allowed)| granted == principal && allowed == object)
    }

    fn create_entity(&self, info: EntityInfo) -> Result<u32, ScriptError> {
        check_transform(info.position, info.rotation)?;
        let id = self.last_entity.get() + 1;
        self.last_entity.set(id);
        self.entities.borrow_mut().insert(id, info);
        self.entity_states
            .borrow_mut()
            .insert(id, EntityState::default());
        Ok(id)
    }

    fn set_entity_owner(&self, entity: u32, owner: u32) -> bool {
        let mut entities = self.entities.borrow_mut();
        let Some(info) = entities.get_mut(&entity) else {
            return false;
        };
        info.owner = (owner != 0).then_some(owner);
        true
    }

    fn set_entity_orphan_mode(&self, entity: u32, mode: &str) -> Result<(), ScriptError> {
        if !matches!(mode, "delete" | "migrate") {
            return Err(ScriptError::Runtime(format!(
                "unknown orphan mode `{mode}`"
            )));
        }
        self.orphan_modes
            .borrow_mut()
            .insert(entity, mode.to_owned());
        Ok(())
    }

    fn delete_entity(&self, entity: u32) -> bool {
        let existed = self.entities.borrow_mut().remove(&entity).is_some();
        self.entity_states.borrow_mut().remove(&entity);
        let bag = entity_bag(entity);
        self.state
            .borrow_mut()
            .retain(|(owner, _), _| *owner != bag);
        existed
    }

    fn set_entity_position(&self, entity: u32, position: [f32; 3]) -> Result<(), ScriptError> {
        check_transform(position, [0.0; 3])?;
        self.entity_slot(entity)?.position = position;
        Ok(())
    }

    fn set_entity_rotation(&self, entity: u32, rotation: [f32; 3]) -> Result<(), ScriptError> {
        check_transform([0.0; 3], rotation)?;
        self.entity_slot(entity)?.rotation = rotation;
        Ok(())
    }

    fn set_entity_frozen(&self, entity: u32, frozen: bool) -> Result<(), ScriptError> {
        if self.entity(entity).is_none() {
            return Err(ScriptError::Runtime(format!(
                "entity {entity} does not exist"
            )));
        }
        self.entity_states
            .borrow_mut()
            .entry(entity)
            .or_default()
            .frozen = frozen;
        Ok(())
    }

    fn set_entity_velocity(&self, entity: u32, velocity: [f32; 3]) -> Result<(), ScriptError> {
        check_transform(velocity, [0.0; 3])?;
        if self.entity(entity).is_none() {
            return Err(ScriptError::Runtime(format!(
                "entity {entity} does not exist"
            )));
        }
        self.entity_states
            .borrow_mut()
            .entry(entity)
            .or_default()
            .velocity = velocity;
        Ok(())
    }

    fn set_entity_health(&self, entity: u32, health: i32) -> Result<(), ScriptError> {
        if self.entity(entity).is_none() {
            return Err(ScriptError::Runtime(format!(
                "entity {entity} does not exist"
            )));
        }
        self.entity_states
            .borrow_mut()
            .entry(entity)
            .or_default()
            .health = health;
        Ok(())
    }

    fn set_entity_max_health(&self, entity: u32, max_health: i32) -> Result<(), ScriptError> {
        if self.entity(entity).is_none() {
            return Err(ScriptError::Runtime(format!(
                "entity {entity} does not exist"
            )));
        }
        self.entity_states
            .borrow_mut()
            .entry(entity)
            .or_default()
            .max_health = max_health;
        Ok(())
    }

    fn set_entity_visible(&self, entity: u32, visible: bool) -> Result<(), ScriptError> {
        if self.entity(entity).is_none() {
            return Err(ScriptError::Runtime(format!(
                "entity {entity} does not exist"
            )));
        }
        self.entity_states
            .borrow_mut()
            .entry(entity)
            .or_default()
            .visible = visible;
        Ok(())
    }

    fn set_entity_alpha(&self, entity: u32, alpha: u8) -> Result<(), ScriptError> {
        if self.entity(entity).is_none() {
            return Err(ScriptError::Runtime(format!(
                "entity {entity} does not exist"
            )));
        }
        self.entity_states
            .borrow_mut()
            .entry(entity)
            .or_default()
            .alpha = alpha;
        Ok(())
    }

    fn attach_entity_to_entity(
        &self,
        entity: u32,
        attachment: EntityAttachment,
    ) -> Result<(), ScriptError> {
        if entity == attachment.parent {
            return Err(ScriptError::Runtime(
                "cannot attach entity to itself".to_owned(),
            ));
        }
        if self.entity(entity).is_none() || self.entity(attachment.parent).is_none() {
            return Err(ScriptError::Runtime("entity does not exist".to_owned()));
        }
        self.entity_states
            .borrow_mut()
            .entry(entity)
            .or_default()
            .attachment = Some(attachment);
        Ok(())
    }

    fn detach_entity(&self, entity: u32) -> Result<(), ScriptError> {
        if self.entity(entity).is_none() {
            return Err(ScriptError::Runtime(format!(
                "entity {entity} does not exist"
            )));
        }
        self.entity_states
            .borrow_mut()
            .entry(entity)
            .or_default()
            .attachment = None;
        Ok(())
    }

    fn set_convar(&self, name: &str, value: &str, kind: ConvarKind) -> Result<(), ScriptError> {
        self.convar_sets
            .borrow_mut()
            .push((name.to_owned(), value.to_owned(), kind));
        Ok(())
    }

    fn player_ped(&self, player: u32) -> Option<u32> {
        self.player_peds.borrow().get(&player).copied()
    }

    fn set_web_handler(&self, resource: &ResourceName, handler: FuncRef) {
        self.web_handlers
            .borrow_mut()
            .push((resource.clone(), handler));
    }

    fn web_reply(&self, request: u64, reply: WebReply) -> Result<(), ScriptError> {
        self.web_replies.borrow_mut().push((request, reply));
        Ok(())
    }

    fn player_routing_bucket(&self, player: u32) -> Option<u32> {
        let connected = self.players.borrow().iter().any(|(id, _)| *id == player);
        connected.then(|| {
            self.player_buckets
                .borrow()
                .get(&player)
                .copied()
                .unwrap_or(0)
        })
    }

    fn set_player_routing_bucket(&self, player: u32, bucket: u32) -> bool {
        if self.player_routing_bucket(player).is_none() {
            return false;
        }
        store_bucket(&mut self.player_buckets.borrow_mut(), player, bucket);
        true
    }

    fn entity_routing_bucket(&self, entity: u32) -> Option<u32> {
        let exists = self.entities.borrow().contains_key(&entity);
        exists.then(|| {
            self.entity_buckets
                .borrow()
                .get(&entity)
                .copied()
                .unwrap_or(0)
        })
    }

    fn set_entity_routing_bucket(&self, entity: u32, bucket: u32) -> bool {
        if self.entity_routing_bucket(entity).is_none() {
            return false;
        }
        store_bucket(&mut self.entity_buckets.borrow_mut(), entity, bucket);
        true
    }

    fn players_in_routing_bucket(&self, bucket: u32) -> Vec<u32> {
        let buckets = self.player_buckets.borrow();
        self.players
            .borrow()
            .iter()
            .map(|(id, _)| *id)
            .filter(|id| buckets.get(id).copied().unwrap_or(0) == bucket)
            .collect()
    }

    fn set_routing_bucket_population_enabled(&self, bucket: u32, enabled: bool) {
        self.population.borrow_mut().insert(bucket, enabled);
    }

    fn routing_bucket_population_enabled(&self, bucket: u32) -> bool {
        self.population
            .borrow()
            .get(&bucket)
            .copied()
            .unwrap_or(true)
    }

    fn set_routing_bucket_entity_lockdown_mode(
        &self,
        bucket: u32,
        mode: &str,
    ) -> Result<(), ScriptError> {
        if !matches!(mode, "inactive" | "relaxed" | "strict") {
            return Err(ScriptError::Runtime(format!(
                "unknown lockdown mode `{mode}`"
            )));
        }
        self.lockdown.borrow_mut().insert(bucket, mode.to_owned());
        Ok(())
    }

    fn routing_bucket_entity_lockdown_mode(&self, bucket: u32) -> String {
        self.lockdown
            .borrow()
            .get(&bucket)
            .cloned()
            .unwrap_or_else(|| "inactive".to_owned())
    }

    fn start_resource(&self, caller: &str, resource: &str) -> bool {
        self.resource_actions
            .borrow_mut()
            .push(("start", caller.to_owned(), resource.to_owned()));
        self.roots.borrow().contains_key(resource)
    }

    fn stop_resource(&self, caller: &str, resource: &str) -> bool {
        self.resource_actions
            .borrow_mut()
            .push(("stop", caller.to_owned(), resource.to_owned()));
        self.roots.borrow().contains_key(resource)
    }

    fn restart_resource(&self, caller: &str, resource: &str) -> bool {
        self.resource_actions.borrow_mut().push((
            "restart",
            caller.to_owned(),
            resource.to_owned(),
        ));
        self.roots.borrow().contains_key(resource)
    }
}

impl TestHost {
    /// Stores an entity of `kind` with the given owner, as the server would
    /// have it, and returns its id. Ids continue those of the other entities.
    pub fn create_entity_for_test(&self, kind: EntityKind, owner: Option<u32>) -> u32 {
        let id = self.last_entity.get() + 1;
        self.last_entity.set(id);
        let info = EntityInfo {
            kind,
            model: 1,
            position: [0.0; 3],
            rotation: [0.0; 3],
            owner,
        };
        self.entities.borrow_mut().insert(id, info);
        id
    }
}

impl TestHost {
    /// Keeps the key-value store in `dir`, so that a later host with the same
    /// folder reads what this one wrote.
    pub fn set_kvp_dir(&self, dir: PathBuf) {
        *self.kvp.borrow_mut() = Some(KvpStore::new(dir));
    }

    /// Writes the key-value changes to the disk and stops the store's writer,
    /// so that a later host with the same folder reads them.
    pub fn close_kvp(&self) {
        self.with_kvp(KvpStore::shutdown);
    }

    /// Runs `f` on the key-value store, opening a temporary one on first use.
    fn with_kvp<T>(&self, f: impl FnOnce(&mut KvpStore) -> T) -> T {
        let mut slot = self.kvp.borrow_mut();
        let store = slot.get_or_insert_with(|| {
            let dir = tempfile::tempdir().expect("temp dir");
            let root = dir.path().to_path_buf();
            self.dirs.borrow_mut().push(dir);
            KvpStore::new(root)
        });
        f(store)
    }

    /// Runs `f` on the entity `entity`, or fails when it does not exist.
    fn entity_slot(&self, entity: u32) -> Result<std::cell::RefMut<'_, EntityInfo>, ScriptError> {
        let entities = self.entities.borrow_mut();
        std::cell::RefMut::filter_map(entities, |map| map.get_mut(&entity))
            .map_err(|_| ScriptError::Runtime(format!("entity {entity} does not exist")))
    }
}

/// Stores `bucket` for `id`, or forgets `id` when it is bucket 0.
fn store_bucket(buckets: &mut BTreeMap<u32, u32>, id: u32, bucket: u32) {
    if bucket == 0 {
        buckets.remove(&id);
    } else {
        buckets.insert(id, bucket);
    }
}

/// Fails unless every component of both transforms is finite.
fn check_transform(position: [f32; 3], rotation: [f32; 3]) -> Result<(), ScriptError> {
    if is_finite(position) && is_finite(rotation) {
        Ok(())
    } else {
        Err(ScriptError::Runtime(
            "entity transforms must be finite".to_owned(),
        ))
    }
}

impl ClientHost for TestHost {
    fn trigger_server_event(&self, event: &str, args: &[Value]) -> Result<(), ScriptError> {
        if args.iter().any(Value::contains_func_ref) {
            return Err(ScriptError::Runtime(
                "function references cannot be sent to the server".to_owned(),
            ));
        }
        self.server_events
            .borrow_mut()
            .push((event.to_owned(), args.to_vec()));
        Ok(())
    }

    fn trigger_latent_server_event(
        &self,
        event: &str,
        args: &[Value],
        bps: u64,
    ) -> Result<(), ScriptError> {
        self.latent_server_events
            .borrow_mut()
            .push((event.to_owned(), args.to_vec(), bps));
        Ok(())
    }

    fn local_player(&self) -> u32 {
        self.local_player
    }

    fn local_ped(&self) -> Option<u32> {
        self.local_ped.get()
    }

    fn create_network_entity(
        &self,
        kind: EntityKind,
        model: u32,
        position: [f32; 3],
        rotation: [f32; 3],
    ) -> Result<u32, ScriptError> {
        check_transform(position, rotation)?;
        let id = self.last_entity.get() + 1;
        self.last_entity.set(id);
        let info = EntityInfo {
            kind,
            model,
            position,
            rotation,
            owner: Some(self.local_player),
        };
        self.entities.borrow_mut().insert(id, info);
        self.created_requests.borrow_mut().push(id);
        Ok(id)
    }

    fn request_entity_control(&self, entity: u32) -> Result<bool, ScriptError> {
        let unowned = self
            .entities
            .borrow()
            .get(&entity)
            .is_some_and(|info| info.owner.is_none());
        if unowned {
            self.control_requests.borrow_mut().push(entity);
        }
        Ok(unowned)
    }

    /// Applies the move to the entity like the client does, and records it.
    /// Only an entity this host owns can be moved.
    fn move_owned_entity(
        &self,
        entity: u32,
        position: Option<[f32; 3]>,
        rotation: Option<[f32; 3]>,
    ) -> Result<(), ScriptError> {
        let mut entities = self.entities.borrow_mut();
        let info = entities
            .get_mut(&entity)
            .filter(|info| info.owner == Some(self.local_player))
            .ok_or_else(|| {
                ScriptError::Runtime(format!("entity {entity} is not owned by this client"))
            })?;
        check_transform(position.unwrap_or([0.0; 3]), rotation.unwrap_or([0.0; 3]))?;
        if let Some(position) = position {
            info.position = position;
        }
        if let Some(rotation) = rotation {
            info.rotation = rotation;
        }
        self.owned_moves
            .borrow_mut()
            .push((entity, position, rotation));
        Ok(())
    }
}

/// Creates a server host with a few convars set.
pub fn new_host() -> Rc<TestHost> {
    let convars = HashMap::from([("sv_maxclients".to_owned(), "32".to_owned())]);
    Rc::new(TestHost {
        convars,
        ..TestHost::default()
    })
}

/// Creates a server host whose convars are exactly `convars`.
pub fn new_host_with_convars(convars: &[(&str, &str)]) -> Rc<TestHost> {
    let convars = convars
        .iter()
        .map(|(name, value)| ((*name).to_owned(), (*value).to_owned()))
        .collect();
    Rc::new(TestHost {
        convars,
        ..TestHost::default()
    })
}

/// Creates a client host whose server id is `local_player`.
pub fn new_client_host(local_player: u32) -> Rc<TestHost> {
    Rc::new(TestHost {
        side: Side::Client,
        local_player,
        ..TestHost::default()
    })
}

/// Writes `scripts` into a fresh folder, creates a runtime for it, registers it
/// with `host`, and runs its scripts in order.
pub fn start(
    host: &Rc<TestHost>,
    name: &str,
    exports: &[&str],
    scripts: &[(&str, &str)],
) -> Rc<JsRuntime> {
    let dir = tempfile::tempdir().expect("temp dir");
    let root = dir.path().to_path_buf();
    for (file, body) in scripts {
        let path = root.join(file);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).expect("script folder");
        }
        fs::write(&path, body).expect("script file");
    }

    let resource: ResourceName = Arc::from(name);
    let instance = host.next_instance.get() + 1;
    host.next_instance.set(instance);
    let concrete: Weak<TestHost> = Rc::downgrade(host);
    let weak: Weak<dyn Host> = concrete;
    let runtime = JsRuntime::new(
        resource,
        instance,
        root.clone(),
        exports.iter().map(|export| (*export).to_owned()).collect(),
        weak,
    )
    .expect("runtime");

    host.runtimes.borrow_mut().push(Rc::clone(&runtime));
    host.roots
        .borrow_mut()
        .insert(name.to_owned(), root.clone());
    host.dirs.borrow_mut().push(dir);
    for (file, _) in scripts {
        if std::path::Path::new(file)
            .extension()
            .is_some_and(|ext| ext.eq_ignore_ascii_case("js"))
        {
            runtime
                .load_script(&root.join(file))
                .unwrap_or_else(|err| panic!("loading {file} in {name}: {err}"));
        }
    }
    runtime
}

/// Runs one frame of every runtime at `now`.
pub fn tick(host: &TestHost, now: Instant) {
    for runtime in host.snapshot() {
        runtime.tick(now);
    }
}

/// Calls an export with no arguments and returns its results.
pub fn call(runtime: &JsRuntime, export: &str, args: &[Value]) -> Vec<Value> {
    runtime
        .call_export(export, args, None)
        .unwrap_or_else(|err| panic!("export `{export}` failed: {err}"))
}

/// Calls an export with no arguments and returns its first result.
pub fn first(runtime: &JsRuntime, export: &str) -> Value {
    call(runtime, export, &[])
        .into_iter()
        .next()
        .unwrap_or(Value::Nil)
}

/// Calls an export that returns a string, as text.
pub fn text(runtime: &JsRuntime, export: &str) -> String {
    match first(runtime, export) {
        Value::String(bytes) => String::from_utf8_lossy(&bytes).into_owned(),
        other => panic!("export `{export}` returned {other:?}, not a string"),
    }
}
