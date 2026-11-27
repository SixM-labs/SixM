//! Routing buckets: separate worlds on one server.
//!
//! Every player and entity is in one bucket, bucket 0 by default. A player holds
//! only the entities of its own bucket, whether or not they are in its scope,
//! and an entity reaches only the players of its bucket. A player's ped moves
//! with the player. Changing a bucket adds and removes entities for the affected
//! clients in the same call.
//!
//! The membership of each bucket lives with the scopes and the entities, where
//! the broadcast and scope code can filter by it. This module holds the entry
//! points the scripts use, and the per-bucket settings that are stored only:
//! the population switch and the entity lockdown mode, which a game adapter
//! and the entity creation rules read later.

use std::collections::BTreeMap;

use sixm_core::ScriptError;

use crate::manager::ResourceManager;

/// The lockdown mode of a bucket that has none set.
const DEFAULT_LOCKDOWN: &str = "inactive";

/// The lockdown modes a bucket may be given.
const LOCKDOWN_MODES: [&str; 3] = [DEFAULT_LOCKDOWN, "relaxed", "strict"];

/// The per-bucket settings. A bucket without an entry has the default value.
#[derive(Debug, Default)]
pub(crate) struct BucketSettings {
    /// Whether the population of each bucket is enabled. Enabled by default.
    population: BTreeMap<u32, bool>,
    /// The entity lockdown mode of each bucket, one of [`LOCKDOWN_MODES`].
    lockdown: BTreeMap<u32, String>,
}

impl BucketSettings {
    /// Stores whether the population of `bucket` is enabled.
    pub(crate) fn set_population(&mut self, bucket: u32, enabled: bool) {
        self.population.insert(bucket, enabled);
    }

    /// Whether the population of `bucket` is enabled.
    pub(crate) fn population_enabled(&self, bucket: u32) -> bool {
        self.population.get(&bucket).copied().unwrap_or(true)
    }

    /// Stores the lockdown mode of `bucket`. Fails for a mode that is not
    /// `inactive`, `relaxed` or `strict`, and then nothing changes.
    pub(crate) fn set_lockdown(&mut self, bucket: u32, mode: &str) -> Result<(), ScriptError> {
        if !LOCKDOWN_MODES.contains(&mode) {
            return Err(ScriptError::Runtime(format!(
                "unknown entity lockdown mode `{mode}`: use inactive, relaxed or strict"
            )));
        }
        self.lockdown.insert(bucket, mode.to_owned());
        Ok(())
    }

    /// The lockdown mode explicitly stored for `bucket`, or `None` when the
    /// bucket falls back to `sv_entityLockdown`.
    pub(crate) fn lockdown_mode(&self, bucket: u32) -> Option<&str> {
        self.lockdown.get(&bucket).map(String::as_str)
    }
}

impl ResourceManager {
    /// The bucket of the connected player `player`, or `None` when it is not
    /// connected.
    pub(crate) fn routed_player(&self, player: u32) -> Option<u32> {
        let scopes = self.scopes.borrow();
        scopes
            .players
            .contains_key(&player)
            .then(|| scopes.buckets.get(&player).copied().unwrap_or(0))
    }

    /// Moves the connected player `player` and its ped to `bucket`. Returns
    /// `false`, and changes nothing, when the player is not connected.
    pub(crate) fn route_player(&self, player: u32, bucket: u32) -> bool {
        if self.routed_player(player).is_none() {
            return false;
        }
        let ped = self.ped_of_player(player);
        self.rebucket(|entities, scopes| {
            if bucket == 0 {
                scopes.buckets.remove(&player);
            } else {
                scopes.buckets.insert(player, bucket);
            }
            if let Some(ped) = ped {
                entities.set_bucket(ped, bucket);
            }
        });
        self.settle_owned_by_bucket(player);
        true
    }

    /// The bucket of the entity `entity`, or `None` when it does not exist.
    pub(crate) fn routed_entity(&self, entity: u32) -> Option<u32> {
        let entities = self.entities.borrow();
        entities.contains(entity).then(|| entities.bucket(entity))
    }

    /// Moves the entity `entity` to `bucket`. Returns `false`, and changes
    /// nothing, when the entity does not exist.
    pub(crate) fn route_entity(&self, entity: u32, bucket: u32) -> bool {
        if !self.entities.borrow().contains(entity) {
            return false;
        }
        self.rebucket(|entities, _| entities.set_bucket(entity, bucket));
        self.settle_owner_of_entity(entity);
        true
    }

    /// The connected players in `bucket`, in ascending order.
    pub(crate) fn players_in_bucket(&self, bucket: u32) -> Vec<u32> {
        let scopes = self.scopes.borrow();
        scopes
            .players
            .keys()
            .copied()
            .filter(|player| scopes.buckets.get(player).copied().unwrap_or(0) == bucket)
            .collect()
    }
}
