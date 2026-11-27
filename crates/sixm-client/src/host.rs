//! The [`Host`] and [`ClientHost`] services of [`ClientRuntime`], which the
//! script runtimes call back into.

use std::fs;
use std::path::PathBuf;

use sixm_core::{
    ClientHost, EntityInfo, EntityKind, EventSource, FuncRef, GLOBAL_BAG, Host, HttpRequest,
    ResourceName, ResourceState, ScriptError, ScriptRuntime, ServerHost, Value, entity_of_bag,
    is_finite, player_of_bag,
};
use tracing::warn;

use crate::runtime::{ClientRuntime, ScriptCommand};
use crate::state::validate_key;

impl Host for ClientRuntime {
    fn server(&self) -> Option<&dyn ServerHost> {
        None
    }

    fn client(&self) -> Option<&dyn ClientHost> {
        Some(self)
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

    fn resource_state(&self, resource: &str) -> ResourceState {
        self.state_of(resource)
    }

    fn resource_names(&self) -> Vec<ResourceName> {
        self.running_names()
    }

    fn resource_metadata(&self, resource: &str, key: &str) -> Vec<String> {
        self.metadata_of(resource, key)
    }

    fn call_ref(&self, func: &FuncRef, args: &[Value]) -> Result<Vec<Value>, ScriptError> {
        let runtime = self
            .runtime_of(func)
            .ok_or_else(|| ScriptError::StaleRef(func.clone()))?;
        runtime.call_ref(func.id, args)
    }

    fn retain_ref(&self, func: &FuncRef) -> bool {
        self.runtime_of(func)
            .is_some_and(|runtime| runtime.retain_ref(func.id))
    }

    fn release_ref(&self, func: &FuncRef) {
        if let Some(runtime) = self.runtime_of(func) {
            runtime.release_ref(func.id);
        }
    }

    /// Refused: `PerformHttpRequest` is a server function, and the client
    /// binds no such global. The callback is released at once.
    fn perform_http_request(
        &self,
        _resource: &ResourceName,
        _request: HttpRequest,
        callback: FuncRef,
    ) -> Result<(), ScriptError> {
        self.release_ref(&callback);
        Err(ScriptError::Runtime(
            "PerformHttpRequest is not available on the client".to_owned(),
        ))
    }

    /// The client runs every line itself, so `_invoker` does not change how it runs.
    fn execute_command(&self, line: &str, _invoker: Option<&ResourceName>) {
        self.queued.borrow_mut().push_back(line.to_owned());
    }

    fn register_command(&self, name: &str, handler: FuncRef, restricted: bool) {
        if !self.retain_ref(&handler) {
            warn!("command `{name}` ignored: its function is no longer valid");
            return;
        }
        let previous = self.commands.borrow_mut().insert(
            name.to_ascii_lowercase(),
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
        self.convars.borrow().get(name).map(str::to_owned)
    }

    /// The synced files live in the content cache under their hashes, so there
    /// is no folder to report for a resource. `LoadResourceFile` goes through
    /// [`load_resource_file`](Host::load_resource_file) instead.
    fn resource_path(&self, _resource: &str) -> Option<PathBuf> {
        None
    }

    /// Reads a synced file from the cache, or any other file from the
    /// resource's data folder. Only a running resource has files.
    fn load_resource_file(&self, resource: &str, path: &str) -> Option<Vec<u8>> {
        if !self.is_running(resource) {
            return None;
        }
        let cached = self
            .files
            .borrow()
            .get(resource)
            .and_then(|files| files.get(path).cloned());
        let file = match cached {
            Some(cache_path) => cache_path,
            None => self.data_path(resource)?.join(path),
        };
        fs::read(file).ok()
    }

    /// Writes into the resource's data folder. A declared file cannot be
    /// overwritten, so a saved file never shadows one the server sent.
    fn save_resource_file(&self, resource: &str, path: &str, data: &[u8]) -> bool {
        if !self.is_running(resource) {
            return false;
        }
        let declared = self
            .files
            .borrow()
            .get(resource)
            .is_some_and(|files| files.contains_key(path));
        if declared {
            return false;
        }
        let Some(dir) = self.data_path(resource) else {
            return false;
        };
        let full = dir.join(path);
        if let Some(parent) = full.parent()
            && fs::create_dir_all(parent).is_err()
        {
            return false;
        }
        fs::write(full, data).is_ok()
    }

    fn state_get(&self, bag: &str, key: &str) -> Option<Value> {
        self.state.borrow().get(bag, key).cloned()
    }

    fn entity(&self, entity: u32) -> Option<EntityInfo> {
        self.entities.borrow().get(&entity).map(|slot| slot.info)
    }

    fn entities(&self, kind: Option<EntityKind>) -> Vec<u32> {
        self.entities
            .borrow()
            .iter()
            .filter(|(_, slot)| kind.is_none_or(|wanted| slot.info.kind == wanted))
            .map(|(id, _)| *id)
            .collect()
    }

    /// Writes local state. Replicated state belongs to the server, so a
    /// replicated write is refused here.
    fn state_set(
        &self,
        bag: &str,
        key: &str,
        value: Value,
        replicated: bool,
    ) -> Result<(), ScriptError> {
        if replicated {
            return Err(ScriptError::Runtime(
                "clients cannot write replicated state".to_owned(),
            ));
        }
        validate_key(key)?;
        if value.contains_func_ref() {
            return Err(ScriptError::Runtime(
                "state values cannot contain functions".to_owned(),
            ));
        }
        if !self.bag_exists(bag) {
            return Err(ScriptError::Runtime(format!(
                "state bag `{bag}` does not exist"
            )));
        }
        self.state.borrow_mut().apply(bag, key, &value);
        self.deliver_state(bag, key, &value, false);
        Ok(())
    }
}

impl ClientRuntime {
    /// Whether a local write to `bag` makes sense: the global bag, this
    /// client's own bag, the bag of a mirrored entity, and any bag the server
    /// has replicated keys into.
    fn bag_exists(&self, bag: &str) -> bool {
        bag == GLOBAL_BAG
            || player_of_bag(bag) == Some(self.local_player)
            || entity_of_bag(bag).is_some_and(|entity| self.entities.borrow().contains_key(&entity))
            || self.state.borrow().has_bag(bag)
    }
}

impl ClientHost for ClientRuntime {
    fn trigger_server_event(&self, event: &str, args: &[Value]) -> Result<(), ScriptError> {
        if args.iter().any(Value::contains_func_ref) {
            return Err(ScriptError::Runtime(
                "function references cannot be sent to the server".to_owned(),
            ));
        }
        self.client
            .borrow()
            .send_event(event, args)
            .map_err(|err| ScriptError::Runtime(err.to_string()))
    }

    fn trigger_latent_server_event(
        &self,
        event: &str,
        args: &[Value],
        bps: u64,
    ) -> Result<(), ScriptError> {
        if args.iter().any(Value::contains_func_ref) {
            return Err(ScriptError::Runtime(
                "function references cannot be sent to the server".to_owned(),
            ));
        }
        self.client
            .borrow()
            .send_latent_event(event, args, bps)
            .map_err(|err| ScriptError::Runtime(err.to_string()))
    }

    fn local_player(&self) -> u32 {
        self.local_player
    }

    fn local_ped(&self) -> Option<u32> {
        self.local_ped.get()
    }

    fn move_owned_entity(
        &self,
        entity: u32,
        position: Option<[f32; 3]>,
        rotation: Option<[f32; 3]>,
    ) -> Result<(), ScriptError> {
        if !position.is_none_or(is_finite) || !rotation.is_none_or(is_finite) {
            return Err(ScriptError::Runtime(
                "entity transforms must be finite".to_owned(),
            ));
        }
        let mut entities = self.entities.borrow_mut();
        let slot = entities
            .get_mut(&entity)
            .ok_or_else(|| ScriptError::Runtime(format!("entity {entity} does not exist")))?;
        if slot.info.owner != Some(self.local_player) {
            return Err(ScriptError::Runtime(format!(
                "entity {entity} is not owned by this client"
            )));
        }
        let queued = slot.is_dirty();
        if let Some(position) = position {
            slot.info.position = position;
            slot.position_dirty = true;
        }
        if let Some(rotation) = rotation {
            slot.info.rotation = rotation;
            slot.rotation_dirty = true;
        }
        if !queued && slot.is_dirty() {
            self.owned_moves.borrow_mut().push(entity);
        }
        Ok(())
    }
}
