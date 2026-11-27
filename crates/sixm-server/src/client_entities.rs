//! Entities that clients create, and who owns each entity.
//!
//! A client creates an entity under an id the server leased to it. The id is
//! final from the start: the client uses it at once, and the server either
//! confirms the entity with `EntityCreated` or refuses it with
//! `EntityCreateRefused`, which makes the client remove it again. Every request
//! takes the next leased id whether it is accepted or not, so both sides stay
//! in step.
//!
//! Ownership moves between players only through the rules here: a client asks
//! for an entity no player owns, a script gives one away, or the owner leaves
//! or moves out of the entity's bucket. The teleport epoch never changes on a
//! move of ownership, so the new owner starts from the current epoch.

use sixm_core::{
    EntityInfo, EntityKind, EventSource, ScriptError, ServerHost, Value, entity_bag, is_finite,
};
use tracing::{debug, info, warn};

use crate::manager::ResourceManager;
use crate::scope::squared_distance;

/// Ids leased to a client each time it runs low.
pub(crate) const LEASE_BLOCK: u32 = 32;

/// A client whose leased ids fall below this many is leased another block.
pub(crate) const LEASE_LOW_WATER: u32 = 16;

/// Ownership rule for an entity whose creator leaves: it is deleted.
pub(crate) const ORPHAN_DELETE: &str = "delete";

/// Ownership rule for an entity whose creator leaves: it is handed on.
pub(crate) const ORPHAN_MIGRATE: &str = "migrate";

/// Why a client's request was refused, as the client hears it.
const LOCKED_DOWN: &str = "entity creation is locked down in this bucket";
const PEDS_LOCKED_DOWN: &str = "clients may not create peds in this bucket";
const NOT_FINITE: &str = "entity transforms must be finite";
const UNKNOWN_KIND: &str = "unknown entity kind";
const CANCELED: &str = "creation was canceled by a script";

impl ResourceManager {
    /// Leases a block of ids to `player` when its own ids run low. Called once
    /// the player is admitted, and after each of its creation requests, so the
    /// new block follows the answer to the request that used the last ids.
    pub(crate) fn top_up_entity_ids(&self, player: u32) {
        let granted = {
            let mut entities = self.entities.borrow_mut();
            if entities.leased_remaining(player) >= LEASE_LOW_WATER {
                return;
            }
            entities.lease_ids(player, LEASE_BLOCK)
        };
        let Some((first, count)) = granted else {
            warn!("no entity ids are left to lease to player {player}");
            return;
        };
        self.with_server(|server| {
            if let Err(err) = server.send_entity_id_lease(player, first, count) {
                warn!("cannot lease entity ids to player {player}: {err}");
            }
        });
    }

    /// Handles `CreateEntity` from `player`. A request that does not use the
    /// player's next leased id ends the connection, since the client's ids are
    /// out of step. Any other refusal is logged and answered with
    /// `EntityCreateRefused`.
    pub(crate) fn client_create_entity(
        &self,
        player: u32,
        id: u32,
        kind: u8,
        model: u32,
        position: [f32; 3],
        rotation: [f32; 3],
    ) {
        if !self.entities.borrow_mut().use_leased_id(player, id) {
            warn!(player, id, "entity {id} was not leased to the player");
            self.queue_drop(player, "entity id out of lease");
            return;
        }
        match self.creation_refusal(player, id, kind, model, position, rotation) {
            Some(reason) => {
                info!(
                    player,
                    id, "refused entity {id} from player {player}: {reason}"
                );
                self.with_server(|server| {
                    if let Err(err) = server.send_entity_create_refused(player, id, &reason) {
                        warn!("cannot refuse entity {id} to player {player}: {err}");
                    }
                });
            }
            None => self.create_for_client(player, id, kind, model, position, rotation),
        }
        self.top_up_entity_ids(player);
    }

    /// Why the request from `player` for the entity `id` is refused, if it is.
    /// The rules are checked in this order: the kind and transform, the bucket's
    /// lockdown, the client's own limit, and then the `entityCreating` handlers,
    /// which may cancel the creation.
    fn creation_refusal(
        &self,
        player: u32,
        id: u32,
        kind: u8,
        model: u32,
        position: [f32; 3],
        rotation: [f32; 3],
    ) -> Option<String> {
        let Some(kind) = EntityKind::from_code(kind) else {
            return Some(UNKNOWN_KIND.to_owned());
        };
        if !is_finite(position) || !is_finite(rotation) {
            return Some(NOT_FINITE.to_owned());
        }
        let bucket = self.bucket_of_player(player);
        match self.routing_bucket_entity_lockdown_mode(bucket).as_str() {
            "strict" => return Some(LOCKED_DOWN.to_owned()),
            "relaxed" if kind == EntityKind::Ped => return Some(PEDS_LOCKED_DOWN.to_owned()),
            _ => {}
        }
        let limit = self.convars.borrow().max_client_entities();
        if self.entities.borrow().created_by(player).len() >= limit {
            return Some(format!(
                "the client already has its maximum of {limit} entities"
            ));
        }
        let canceled = self.dispatch(
            "entityCreating",
            EventSource::Player(player),
            &[
                Value::Int(i64::from(id)),
                Value::Int(i64::from(kind.code())),
                Value::Int(i64::from(model)),
            ],
        );
        canceled.then(|| CANCELED.to_owned())
    }

    /// Stores the accepted entity `id`, tells every client that can see it and
    /// runs `entityCreated`.
    fn create_for_client(
        &self,
        player: u32,
        id: u32,
        kind: u8,
        model: u32,
        position: [f32; 3],
        rotation: [f32; 3],
    ) {
        let Some(kind) = EntityKind::from_code(kind) else {
            return;
        };
        let info = EntityInfo {
            kind,
            model,
            position,
            rotation,
            owner: Some(player),
        };
        let bucket = self.bucket_of_player(player);
        let stored = {
            let mut entities = self.entities.borrow_mut();
            entities
                .insert_created(id, info, player)
                .map(|()| entities.set_bucket(id, bucket))
        };
        if let Err(err) = stored {
            // The id was leased, so the refusal is answered with it.
            self.with_server(|server| {
                if let Err(send) = server.send_entity_create_refused(player, id, &err.to_string()) {
                    warn!("cannot refuse entity {id} to player {player}: {send}");
                }
            });
            return;
        }
        self.state.borrow_mut().create_bag(&entity_bag(id));
        self.announce_created(id, info);
        debug!(player, id, "player {player} created entity {id}");
        self.dispatch(
            "entityCreated",
            EventSource::Local,
            &[Value::Int(i64::from(id))],
        );
    }

    /// Handles `RequestEntityControl` from `player`. The entity goes to the
    /// player when no player owns it and the player holds it. Any other request
    /// is ignored, and the client learns the owner from `EntityOwner`.
    pub(crate) fn client_request_control(&self, player: u32, id: u32) {
        let eligible = {
            let entities = self.entities.borrow();
            entities.get(id).is_some_and(|info| info.owner.is_none())
        };
        if eligible && self.in_scope(player, id) {
            self.change_owner(id, Some(player));
        } else {
            debug!(
                player,
                id, "control of entity {id} not granted to player {player}"
            );
        }
    }

    /// Gives the entity `id` to `owner`, or to the server with `None`, and
    /// tells the players that hold it. Returns `false` when nothing changed or
    /// the entity is unknown.
    pub(crate) fn change_owner(&self, id: u32, owner: Option<u32>) -> bool {
        if !self.entities.borrow_mut().set_owner(id, owner) {
            return false;
        }
        self.announce_owner(id);
        true
    }

    /// Tells every player that holds the entity `id` who owns it now, with the
    /// entity's epoch and transform. The new owner starts from that epoch.
    fn announce_owner(&self, id: u32) {
        let Some((info, epoch)) = self.entities.borrow().info_and_epoch(id) else {
            return;
        };
        let holders: Vec<u32> = self.scopes.borrow().players.keys().copied().collect();
        for player in holders {
            if !self.in_scope(player, id) {
                continue;
            }
            self.with_server(|server| {
                let sent = server.send_entity_owner(
                    player,
                    id,
                    info.owner.unwrap_or(0),
                    epoch,
                    info.position,
                    info.rotation,
                );
                if let Err(err) = sent {
                    warn!("cannot send the owner of entity {id} to player {player}: {err}");
                }
            });
        }
    }

    /// Runs when `player` leaves, before it is removed from the scopes. Entities
    /// it created are deleted, unless they migrate, and then they are handed on.
    /// Entities it owns go to a player that holds them, or to the server. The
    /// leases of the player are dropped.
    pub(crate) fn release_entities_of(&self, player: u32) {
        let created = self.entities.borrow().created_by(player).to_vec();
        for id in created {
            if self.entities.borrow().migrates_on_orphan(id) {
                self.entities.borrow_mut().release_creator(id);
                self.settle_owner_of_leaver(id, player);
            } else {
                self.remove_entity(id);
            }
        }
        let owned = self.entities.borrow().owned(player).to_vec();
        for id in owned {
            self.settle_owner_of_leaver(id, player);
        }
        self.entities.borrow_mut().drop_leases(player);
    }

    /// Hands on the entities `player` owns that are no longer in its bucket,
    /// after it moved to another one.
    pub(crate) fn settle_owned_by_bucket(&self, player: u32) {
        let bucket = self.bucket_of_player(player);
        let owned = self.entities.borrow().owned(player).to_vec();
        for id in owned {
            let outside = {
                let entities = self.entities.borrow();
                !entities.in_bucket(id, bucket)
            };
            if outside {
                self.settle_owner_of_leaver(id, player);
            }
        }
    }

    /// Hands on the entity `id` when its owner has moved it out of its bucket.
    pub(crate) fn settle_owner_of_entity(&self, id: u32) {
        let owner = self.entities.borrow().get(id).and_then(|info| info.owner);
        let Some(owner) = owner else {
            return;
        };
        if !self
            .entities
            .borrow()
            .in_bucket(id, self.bucket_of_player(owner))
        {
            self.settle_owner_of_leaver(id, owner);
        }
    }

    /// Gives the entity `id` away from `leaver`, if `leaver` still owns it, to
    /// the nearest player that holds it, or to the server when none does.
    fn settle_owner_of_leaver(&self, id: u32, leaver: u32) {
        let owned_by_leaver = self
            .entities
            .borrow()
            .get(id)
            .is_some_and(|info| info.owner == Some(leaver));
        if !owned_by_leaver {
            return;
        }
        let target = self.migration_target(id, leaver);
        self.change_owner(id, target);
    }

    /// The player that takes the entity `id` from `leaver`: the holder nearest
    /// to the entity, where a player without a ped is farthest away. `None` when
    /// no other player holds it.
    fn migration_target(&self, id: u32, leaver: u32) -> Option<u32> {
        let at = self.entities.borrow().position(id)?;
        let candidates: Vec<u32> = self
            .scopes
            .borrow()
            .players
            .keys()
            .copied()
            .filter(|&player| player != leaver && self.in_scope(player, id))
            .collect();
        candidates.into_iter().min_by(|&a, &b| {
            self.distance_to_ped(a, at)
                .total_cmp(&self.distance_to_ped(b, at))
                .then(a.cmp(&b))
        })
    }

    /// The squared distance from `at` to the ped of `player`, or infinity when
    /// the player has no ped.
    fn distance_to_ped(&self, player: u32, at: [f32; 3]) -> f32 {
        self.ped_of_player(player)
            .and_then(|ped| self.entities.borrow().position(ped))
            .map_or(f32::INFINITY, |position| squared_distance(position, at))
    }

    /// Gives the entity to `owner` for a script's `SetEntityOwner`, where 0 is
    /// the server. The player must be connected and in the entity's bucket.
    /// Returns `false` when it cannot be done.
    pub(crate) fn script_set_owner(&self, entity: u32, owner: u32) -> bool {
        if !self.entities.borrow().contains(entity) {
            return false;
        }
        if owner == 0 {
            self.change_owner(entity, None);
            return true;
        }
        let connected = self.scopes.borrow().players.contains_key(&owner);
        let same_bucket = self
            .entities
            .borrow()
            .in_bucket(entity, self.bucket_of_player(owner));
        if !connected || !same_bucket {
            return false;
        }
        self.change_owner(entity, Some(owner));
        true
    }

    /// Sets the orphan rule of the entity `id` for a script: `delete` or
    /// `migrate`. Fails for any other mode.
    pub(crate) fn script_set_orphan_mode(
        &self,
        entity: u32,
        mode: &str,
    ) -> Result<(), ScriptError> {
        let migrate = match mode {
            ORPHAN_DELETE => false,
            ORPHAN_MIGRATE => true,
            other => {
                return Err(ScriptError::Runtime(format!(
                    "unknown orphan mode `{other}`: use {ORPHAN_DELETE} or {ORPHAN_MIGRATE}"
                )));
            }
        };
        self.entities
            .borrow_mut()
            .set_migrates_on_orphan(entity, migrate);
        Ok(())
    }
}
