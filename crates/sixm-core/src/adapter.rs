//! Game adapter contract: bridges [`ClientRuntime`](crate) to a concrete or
//! simulated game loop without tying the core to any specific engine.

use std::collections::HashMap;
use std::fmt;
use std::rc::{Rc, Weak};
use std::time::Instant;

use crate::entity::{EntityInfo, EntityState};
use crate::hash::joaat;
use crate::runtime::{EventSource, Host, ScriptError};
use crate::value::Value;

/// Function implemented by a game adapter and callable by client scripts.
pub type NativeFn = Rc<dyn Fn(&[Value]) -> Result<Vec<Value>, ScriptError>>;

/// Why an adapter could not apply a host request. Non-fatal to the client.
#[derive(Debug, thiserror::Error)]
pub enum AdapterError {
    #[error("the game is not ready")]
    NotReady,
    #[error("unknown model {0:#010x}")]
    UnknownModel(u32),
    #[error("{0}")]
    Game(String),
}

/// One change in the client's entity mirror that the adapter should reflect.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum MirrorChange {
    /// An entity entered the client mirror.
    Created { id: u32, info: EntityInfo },
    /// An entity moved. `teleport` is true when an epoch jumped or a script
    /// called `SetEntityCoords`/`SetEntityRotation` directly, so the adapter
    /// should snap without interpolating.
    Moved {
        id: u32,
        info: EntityInfo,
        teleport: bool,
    },
    /// Ownership of an entity changed.
    OwnerChanged { id: u32, info: EntityInfo },
    /// Replicated entity state (`frozen`, `velocity`, health, visibility,
    /// alpha, attachment) changed.
    StateChanged { id: u32, state: EntityState },
    /// An entity left the mirror.
    Removed { id: u32 },
}

/// Registry of game-specific native functions exported by an adapter.
#[derive(Clone, Default)]
pub struct NativeRegistry {
    entries: Vec<(String, NativeFn)>,
    by_name: HashMap<String, usize>,
    by_hash: HashMap<u32, usize>,
}

impl NativeRegistry {
    /// Registers a native function under `name` and indexes its JOAAT hash.
    ///
    /// Fails if `name` is empty, not a valid ASCII identifier, or collides with
    /// an already-registered name or hash.
    pub fn register(
        &mut self,
        name: impl Into<String>,
        function: impl Fn(&[Value]) -> Result<Vec<Value>, ScriptError> + 'static,
    ) -> Result<(), AdapterError> {
        let name = name.into();
        if !is_valid_native_name(&name) {
            return Err(AdapterError::Game(format!("invalid native name `{name}`")));
        }
        if self.by_name.contains_key(&name) {
            return Err(AdapterError::Game(format!(
                "native `{name}` is already registered"
            )));
        }
        let hash = joaat(name.as_bytes());
        if let Some(&existing) = self.by_hash.get(&hash) {
            return Err(AdapterError::Game(format!(
                "native `{name}` hash {hash:#010x} collides with `{}`",
                self.entries[existing].0
            )));
        }
        let index = self.entries.len();
        self.entries.push((name.clone(), Rc::new(function)));
        self.by_name.insert(name, index);
        self.by_hash.insert(hash, index);
        Ok(())
    }

    /// Looks up a registered native by exact name.
    pub fn get(&self, name: &str) -> Option<&NativeFn> {
        self.by_name.get(name).map(|&idx| &self.entries[idx].1)
    }

    /// Looks up a registered native by its `GetHashKey` (JOAAT) hash.
    pub fn get_by_hash(&self, hash: u32) -> Option<&NativeFn> {
        self.by_hash.get(&hash).map(|&idx| &self.entries[idx].1)
    }

    /// Iterates `(name, fn)` pairs in registration order.
    pub fn iter(&self) -> impl Iterator<Item = (&str, &NativeFn)> {
        self.entries
            .iter()
            .map(|(name, func)| (name.as_str(), func))
    }

    /// Number of registered natives.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether the registry has no natives.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

impl fmt::Debug for NativeRegistry {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("NativeRegistry")
            .field("count", &self.entries.len())
            .finish_non_exhaustive()
    }
}

fn is_valid_native_name(name: &str) -> bool {
    let mut bytes = name.bytes();
    let Some(first) = bytes.next() else {
        return false;
    };
    (first.is_ascii_alphabetic() || first == b'_')
        && bytes.all(|b| b.is_ascii_alphanumeric() || b == b'_')
}

/// Main-thread handle allowing an attached adapter to inspect the client
/// mirror and raise local script events.
#[derive(Clone)]
pub struct AdapterSink {
    host: Weak<dyn Host>,
}

impl AdapterSink {
    /// Creates a sink backed by `host`.
    pub fn new(host: Weak<dyn Host>) -> Self {
        Self { host }
    }

    /// Triggers a local event across all running client script runtimes.
    pub fn trigger_event(&self, event: &str, args: &[Value]) -> Result<bool, ScriptError> {
        let host = self
            .host
            .upgrade()
            .ok_or_else(|| ScriptError::Runtime("client host is no longer running".to_owned()))?;
        host.trigger_event(event, EventSource::Local, args, None)
    }

    /// Snapshot of a mirrored entity, if present.
    pub fn entity(&self, id: u32) -> Option<EntityInfo> {
        self.host.upgrade()?.entity(id)
    }

    /// Snapshot of a mirrored entity's replicated state, if present.
    pub fn entity_state(&self, id: u32) -> Option<EntityState> {
        self.host.upgrade()?.entity_state(id)
    }

    /// Reads a state bag entry from the client mirror.
    pub fn state_get(&self, bag: &str, key: &str) -> Option<Value> {
        self.host.upgrade()?.state_get(bag, key)
    }

    /// Local player's server id, if the host is active.
    pub fn local_player(&self) -> Option<u32> {
        Some(self.host.upgrade()?.client()?.local_player())
    }

    /// Local player's ped entity id once created by the server.
    pub fn local_ped(&self) -> Option<u32> {
        self.host.upgrade()?.client()?.local_ped()
    }
}

impl fmt::Debug for AdapterSink {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AdapterSink").finish_non_exhaustive()
    }
}

/// Bridge between the headless `ClientRuntime` mirror and a concrete or
/// simulated game loop. All methods are invoked on the client main thread.
pub trait GameAdapter {
    /// Called once after the initial server snapshot is applied and before any
    /// resource starts. Returning an error aborts client startup.
    fn attach(&mut self, sink: AdapterSink) -> Result<(), AdapterError>;

    /// Called once on disconnect or shutdown after all resources stop.
    fn detach(&mut self);

    /// Called once per frame at `now`, before script runtimes tick.
    fn before_scripts(&mut self, now: Instant);

    /// Called once per frame after scripts tick and before `read_owned`.
    fn after_scripts(&mut self, now: Instant);

    /// Applies queued mirror changes in arrival order. Failures for individual
    /// entities should be pushed into `errors` without aborting the batch.
    fn apply_changes(&mut self, changes: &[MirrorChange], errors: &mut Vec<(u32, AdapterError)>);

    /// Appends `(id, position, rotation)` for each owned entity in `owned`
    /// whose transform moved since the last read.
    fn read_owned(&mut self, owned: &[u32], out: &mut Vec<(u32, [f32; 3], [f32; 3])>);

    /// Game-specific native functions provided by this adapter.
    fn natives(&self) -> &NativeRegistry;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registry_validates_names_and_indexes_hashes() {
        let mut reg = NativeRegistry::default();
        assert!(reg.is_empty());

        reg.register("GetSampleValue", |args| Ok(args.to_vec()))
            .expect("valid native");
        assert_eq!(reg.len(), 1);
        assert!(reg.get("GetSampleValue").is_some());
        assert!(reg.get_by_hash(joaat(b"GetSampleValue")).is_some());
        assert!(reg.get_by_hash(joaat(b"getsamplevalue")).is_some());

        assert!(reg.register("GetSampleValue", |_| Ok(Vec::new())).is_err());
        assert!(reg.register("getsamplevalue", |_| Ok(Vec::new())).is_err());
        assert!(reg.register("", |_| Ok(Vec::new())).is_err());
        assert!(reg.register("123bad", |_| Ok(Vec::new())).is_err());
        assert!(reg.register("bad-name", |_| Ok(Vec::new())).is_err());
    }
}
