//! State bags: the server's copy of every bag, replication to clients, and the
//! change handlers that run on every write.
//!
//! There is one bag named `global`, and one `player:<id>` bag per connected
//! player. A player's bag is created just before `playerJoining` and removed
//! after `playerDropped`, so a write to it outside that window is an error.

use std::collections::HashMap;

use sixm_core::{
    GLOBAL_BAG, MAX_STATE_NAME_LEN, ScriptError, ScriptRuntime, Value, player_bag, player_of_bag,
};
use tracing::warn;

use crate::manager::ResourceManager;

/// A stored value and whether clients are sent it.
#[derive(Debug, Clone, PartialEq)]
struct Entry {
    value: Value,
    replicated: bool,
}

/// Every bag and its keys. A bag is created by its first write, or for a
/// player when the player joins, and a player's bag is removed when the player
/// leaves.
#[derive(Debug, Default)]
pub(crate) struct StateStore {
    bags: HashMap<String, HashMap<String, Entry>>,
}

impl StateStore {
    /// The stored value of `key` in `bag`.
    pub(crate) fn get(&self, bag: &str, key: &str) -> Option<&Value> {
        self.bags.get(bag)?.get(key).map(|entry| &entry.value)
    }

    /// Stores `value` under `key`, or deletes the key when `value` is nil.
    fn apply(&mut self, bag: &str, key: &str, value: &Value, replicated: bool) {
        let keys = self.bags.entry(bag.to_owned()).or_default();
        if matches!(value, Value::Nil) {
            keys.remove(key);
        } else {
            keys.insert(
                key.to_owned(),
                Entry {
                    value: value.clone(),
                    replicated,
                },
            );
        }
    }

    /// Every replicated key of every bag, as `(bag, key, value)`.
    pub(crate) fn replicated(&self) -> Vec<(String, String, Value)> {
        self.bags
            .iter()
            .flat_map(|(bag, keys)| {
                keys.iter()
                    .filter(|(_, entry)| entry.replicated)
                    .map(move |(key, entry)| (bag.clone(), key.clone(), entry.value.clone()))
            })
            .collect()
    }

    /// The names of the bags that exist, sorted.
    pub(crate) fn bag_names(&self) -> Vec<String> {
        let mut names: Vec<String> = self.bags.keys().cloned().collect();
        names.sort();
        names
    }

    /// The keys of `bag` as `(key, value, replicated)`, sorted by key. `None`
    /// when the bag does not exist.
    pub(crate) fn entries(&self, bag: &str) -> Option<Vec<(String, Value, bool)>> {
        let keys = self.bags.get(bag)?;
        let mut entries: Vec<(String, Value, bool)> = keys
            .iter()
            .map(|(key, entry)| (key.clone(), entry.value.clone(), entry.replicated))
            .collect();
        entries.sort_by(|a, b| a.0.cmp(&b.0));
        Some(entries)
    }
}

/// Fails unless `key` is 1 to [`MAX_STATE_NAME_LEN`] bytes long.
fn validate_key(key: &str) -> Result<(), ScriptError> {
    if key.is_empty() || key.len() > MAX_STATE_NAME_LEN {
        return Err(ScriptError::Runtime(format!(
            "state key must be 1 to {MAX_STATE_NAME_LEN} bytes, got {}",
            key.len()
        )));
    }
    Ok(())
}

impl ResourceManager {
    /// Validates and applies one write, from a script or the console.
    ///
    /// A replicated write is encoded and sent before it is stored, so a write
    /// that cannot be sent changes nothing. Every write then runs the change
    /// handlers of every running resource, in start order.
    pub(crate) fn write_state(
        &self,
        bag: &str,
        key: &str,
        value: &Value,
        replicated: bool,
    ) -> Result<(), ScriptError> {
        validate_key(key)?;
        self.ensure_bag(bag)?;
        if value.contains_func_ref() {
            return Err(ScriptError::Runtime(
                "state values cannot contain functions".to_owned(),
            ));
        }
        if replicated {
            self.broadcast_state(bag, key, value)?;
        }
        self.state.borrow_mut().apply(bag, key, value, replicated);
        for runtime in self.snapshot().iter() {
            runtime.deliver_state_change(bag, key, value, replicated);
        }
        Ok(())
    }

    /// Fails unless `bag` is `global` or the bag of a connected player.
    fn ensure_bag(&self, bag: &str) -> Result<(), ScriptError> {
        let exists = bag == GLOBAL_BAG
            || player_of_bag(bag)
                .is_some_and(|player| self.network.players.borrow().contains(player));
        if exists {
            Ok(())
        } else {
            Err(ScriptError::Runtime(format!(
                "state bag `{bag}` does not exist"
            )))
        }
    }

    /// Sends a replicated write to every accepted player. Does nothing while
    /// the network is not running.
    fn broadcast_state(&self, bag: &str, key: &str, value: &Value) -> Result<(), ScriptError> {
        let server = self.network.server.borrow();
        let Some(server) = server.as_ref() else {
            return Ok(());
        };
        server
            .broadcast_state(bag, key, value)
            .map_err(|err| ScriptError::Runtime(err.to_string()))
    }

    /// The names of every state bag, sorted.
    pub(crate) fn state_bag_names(&self) -> Vec<String> {
        self.state.borrow().bag_names()
    }

    /// The keys of `bag` as `(key, value, replicated)`, or `None` for an unknown bag.
    pub(crate) fn state_bag_entries(&self, bag: &str) -> Option<Vec<(String, Value, bool)>> {
        self.state.borrow().entries(bag)
    }

    /// Creates the bag of `player`, which is being admitted, and sends the
    /// player every replicated key of every bag.
    pub(crate) fn join_state(&self, player: u32) {
        let snapshot = {
            let mut store = self.state.borrow_mut();
            store.bags.entry(player_bag(player)).or_default();
            store.replicated()
        };
        let entries: Vec<(&str, &str, &Value)> = snapshot
            .iter()
            .map(|(bag, key, value)| (bag.as_str(), key.as_str(), value))
            .collect();
        self.with_server(|server| {
            if let Err(err) = server.send_state_snapshot(player, &entries) {
                warn!("cannot send the state snapshot to player {player}: {err}");
            }
        });
    }

    /// Removes the bag of `player`, who has left, and tells every player.
    pub(crate) fn leave_state(&self, player: u32) {
        let bag = player_bag(player);
        self.state.borrow_mut().bags.remove(&bag);
        self.with_server(|server| {
            if let Err(err) = server.broadcast_state_removed(&bag) {
                warn!("cannot announce the removal of state bag `{bag}`: {err}");
            }
        });
    }
}
