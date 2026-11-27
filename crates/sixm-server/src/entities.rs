//! Entities: their ids, their transforms, and the transform updates due for
//! clients at the end of each frame.
//!
//! Scripts create, move and delete entities. Creation and deletion reach clients
//! at once. A move only marks the entity, and each marked entity is sent once
//! per frame with its latest transform. The owner of an entity may move it too,
//! and then the other clients receive the owner's transform.

use std::collections::BTreeMap;

use sixm_core::{
    EntityInfo, EntityKind, EventSource, MAX_ENTITIES, ScriptError, Value, entity_bag, is_finite,
    joaat,
};
use sixm_net::EntityEntry;
use tracing::{debug, warn};

use crate::manager::ResourceManager;

/// One transform as clients receive it: the entity id, its position, its
/// rotation, and its teleport epoch.
pub(crate) type EntityUpdate = (u32, [f32; 3], [f32; 3], u32);

/// An entity, its teleport epoch, and whether its transform is queued for the
/// next frame.
#[derive(Debug, Clone, Copy)]
struct Slot {
    info: EntityInfo,
    /// How many times the server has moved the entity itself. Only the server
    /// changes it, and an owner's update must carry the current value.
    epoch: u32,
    dirty: bool,
}

/// Every entity on the server, by id, and the ids whose transform changed since
/// the last frame.
#[derive(Debug)]
pub(crate) struct EntityStore {
    /// The next id to hand out. Zero once every id has been used.
    next_id: u32,
    by_id: BTreeMap<u32, Slot>,
    /// Ids queued for the next frame, each listed once. An id whose entity was
    /// deleted stays here until the frame, and is then skipped.
    dirty: Vec<u32>,
}

impl Default for EntityStore {
    fn default() -> Self {
        Self {
            next_id: 1,
            by_id: BTreeMap::new(),
            dirty: Vec::new(),
        }
    }
}

impl EntityStore {
    /// Stores `info` as a new entity and returns its id. Fails for a transform
    /// that is not finite, when [`MAX_ENTITIES`] entities exist, or when no ids are left.
    pub(crate) fn insert(&mut self, info: EntityInfo) -> Result<u32, ScriptError> {
        check_finite(info.position)?;
        check_finite(info.rotation)?;
        if self.by_id.len() >= MAX_ENTITIES {
            return Err(ScriptError::Runtime(format!(
                "at most {MAX_ENTITIES} entities can exist"
            )));
        }
        let id = self.next_id;
        if id == 0 {
            return Err(ScriptError::Runtime("no entity ids are left".to_owned()));
        }
        self.next_id = id.checked_add(1).unwrap_or(0);
        self.by_id.insert(
            id,
            Slot {
                info,
                epoch: 0,
                dirty: false,
            },
        );
        Ok(id)
    }

    /// Removes the entity `id`, and returns it when it existed.
    pub(crate) fn remove(&mut self, id: u32) -> Option<EntityInfo> {
        self.by_id.remove(&id).map(|slot| slot.info)
    }

    /// The entity `id`, if it exists.
    pub(crate) fn get(&self, id: u32) -> Option<EntityInfo> {
        self.by_id.get(&id).map(|slot| slot.info)
    }

    /// Whether the entity `id` exists.
    pub(crate) fn contains(&self, id: u32) -> bool {
        self.by_id.contains_key(&id)
    }

    /// Every entity with its epoch, in ascending id order.
    pub(crate) fn all(&self) -> Vec<(u32, EntityInfo, u32)> {
        self.by_id
            .iter()
            .map(|(id, slot)| (*id, slot.info, slot.epoch))
            .collect()
    }

    /// The ids of every entity of `kind`, or of every kind when `kind` is `None`,
    /// in ascending order.
    pub(crate) fn ids(&self, kind: Option<EntityKind>) -> Vec<u32> {
        self.by_id
            .iter()
            .filter(|(_, slot)| kind.is_none_or(|wanted| slot.info.kind == wanted))
            .map(|(id, _)| *id)
            .collect()
    }

    /// Moves the entity `id` to `position` on the server, and queues it for the
    /// next frame with a new epoch.
    pub(crate) fn set_position(&mut self, id: u32, position: [f32; 3]) -> Result<(), ScriptError> {
        check_finite(position)?;
        self.update(id, |info| info.position = position)
    }

    /// Rotates the entity `id` on the server, and queues it for the next frame
    /// with a new epoch.
    pub(crate) fn set_rotation(&mut self, id: u32, rotation: [f32; 3]) -> Result<(), ScriptError> {
        check_finite(rotation)?;
        self.update(id, |info| info.rotation = rotation)
    }

    /// Applies `change` to the entity `id` as a server move: the epoch goes up,
    /// so every owner update sent before it is refused, and the entity is queued.
    fn update(&mut self, id: u32, change: impl FnOnce(&mut EntityInfo)) -> Result<(), ScriptError> {
        let slot = self
            .by_id
            .get_mut(&id)
            .ok_or_else(|| ScriptError::Runtime(format!("entity {id} does not exist")))?;
        change(&mut slot.info);
        slot.epoch = slot.epoch.wrapping_add(1);
        if !slot.dirty {
            slot.dirty = true;
            self.dirty.push(id);
        }
        Ok(())
    }

    /// Stores a transform that `player`, the owner of the entity `id`, sent for
    /// the teleport epoch `epoch`. Returns `false` and changes nothing unless the
    /// entity exists, `player` owns it, every component is finite, and `epoch`
    /// is the entity's current epoch. An update computed before a server move
    /// therefore never lands, whenever it arrives. An accepted transform keeps
    /// the epoch.
    pub(crate) fn owner_move(
        &mut self,
        player: u32,
        id: u32,
        position: [f32; 3],
        rotation: [f32; 3],
        epoch: u32,
    ) -> bool {
        let Some(slot) = self.by_id.get_mut(&id) else {
            return false;
        };
        if slot.info.owner != Some(player)
            || slot.epoch != epoch
            || !is_finite(position)
            || !is_finite(rotation)
        {
            return false;
        }
        slot.info.position = position;
        slot.info.rotation = rotation;
        if !slot.dirty {
            slot.dirty = true;
            self.dirty.push(id);
        }
        true
    }

    /// Moves the transform of every queued entity into `out`, in the order the
    /// entities were first moved, and clears the queue. The entries already in
    /// `out` are kept.
    pub(crate) fn take_dirty(&mut self, out: &mut Vec<EntityUpdate>) {
        for id in self.dirty.drain(..) {
            if let Some(slot) = self.by_id.get_mut(&id) {
                slot.dirty = false;
                out.push((id, slot.info.position, slot.info.rotation, slot.epoch));
            }
        }
    }
}

/// Fails unless every component is finite.
fn check_finite(components: [f32; 3]) -> Result<(), ScriptError> {
    if is_finite(components) {
        Ok(())
    } else {
        Err(ScriptError::Runtime(
            "entity transforms must be finite".to_owned(),
        ))
    }
}

impl ResourceManager {
    /// Creates an entity, tells every client and runs `entityCreated`. Its state
    /// bag exists from now on.
    pub(crate) fn spawn_entity(&self, info: EntityInfo) -> Result<u32, ScriptError> {
        let id = self.entities.borrow_mut().insert(info)?;
        self.state.borrow_mut().create_bag(&entity_bag(id));
        let entry = EntityEntry::from((id, info, 0));
        self.with_server(|server| {
            if let Err(err) = server.broadcast_entity_created(entry) {
                warn!("cannot announce entity {id}: {err}");
            }
        });
        self.dispatch(
            "entityCreated",
            EventSource::Local,
            &[Value::Int(i64::from(id))],
        );
        Ok(id)
    }

    /// Deletes the entity `id`. `entityRemoved` runs while it still exists, then
    /// its state bag is removed and every client is told. Returns `false` when
    /// it does not exist, including when a handler of `entityRemoved` deleted it.
    pub(crate) fn remove_entity(&self, id: u32) -> bool {
        let exists = self.entities.borrow().contains(id);
        if !exists {
            return false;
        }
        self.dispatch(
            "entityRemoved",
            EventSource::Local,
            &[Value::Int(i64::from(id))],
        );
        let removed = self.entities.borrow_mut().remove(id);
        if removed.is_none() {
            return false;
        }
        let bag = entity_bag(id);
        self.state.borrow_mut().remove_bag(&bag);
        self.with_server(|server| {
            if let Err(err) = server.broadcast_state_removed(&bag) {
                warn!("cannot announce the removal of state bag `{bag}`: {err}");
            }
            if let Err(err) = server.broadcast_entity_deleted(id) {
                warn!("cannot announce the deletion of entity {id}: {err}");
            }
        });
        true
    }

    /// Creates the ped of player `player`, owned by it, at the origin, and
    /// returns its id. Its model is the `sixm_playerModel` convar. Clients see
    /// it like any entity, and it is deleted again when the player leaves.
    pub(crate) fn spawn_player_ped(&self, player: u32) -> Option<u32> {
        let model = joaat(self.convars.borrow().player_model().as_bytes());
        let info = EntityInfo {
            kind: EntityKind::Ped,
            model,
            position: [0.0; 3],
            rotation: [0.0; 3],
            owner: Some(player),
        };
        match self.spawn_entity(info) {
            Ok(id) => Some(id),
            Err(err) => {
                warn!("cannot create the ped of player {player}: {err}");
                None
            }
        }
    }

    /// Applies the transforms a client sent for the entities it owns. A
    /// transform that breaks the rules is ignored, and is only logged at debug
    /// level, since a client may race a server move.
    pub(crate) fn player_transforms(
        &self,
        player: u32,
        updates: &[(u32, [f32; 3], [f32; 3], u32)],
    ) {
        let ignored = {
            let mut entities = self.entities.borrow_mut();
            updates
                .iter()
                .filter(|&&(id, position, rotation, epoch)| {
                    !entities.owner_move(player, id, position, rotation, epoch)
                })
                .count()
        };
        if ignored > 0 {
            debug!(player, ignored, "ignored entity transforms it may not send");
        }
    }

    /// Sends the transforms queued since the last frame, one update per entity.
    /// Nothing is sent when no entity moved. The update buffer is reused, so a
    /// frame does not allocate once the buffer has grown.
    pub(crate) fn flush_entities(&self) {
        let mut updates = std::mem::take(&mut *self.entity_updates.borrow_mut());
        self.entities.borrow_mut().take_dirty(&mut updates);
        if !updates.is_empty() {
            self.with_server(|server| {
                if let Err(err) = server.broadcast_entity_transforms(&updates) {
                    warn!("cannot send the entity transforms: {err}");
                }
            });
        }
        updates.clear();
        *self.entity_updates.borrow_mut() = updates;
    }

    /// Every entity, in ascending id order, for the console and for joining players.
    pub(crate) fn entity_list(&self) -> Vec<(u32, EntityInfo, u32)> {
        self.entities.borrow().all()
    }

    /// Sends every existing entity to `player`, who is being admitted.
    pub(crate) fn join_entities(&self, player: u32) {
        let entries: Vec<EntityEntry> = self
            .entity_list()
            .into_iter()
            .map(EntityEntry::from)
            .collect();
        self.with_server(|server| {
            if let Err(err) = server.send_entity_snapshot(player, &entries) {
                warn!("cannot send the entity snapshot to player {player}: {err}");
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn info(kind: EntityKind, x: f32) -> EntityInfo {
        EntityInfo {
            kind,
            model: 1,
            position: [x, 0.0, 0.0],
            rotation: [0.0, 0.0, 0.0],
            owner: None,
        }
    }

    fn owned_by(player: u32) -> EntityInfo {
        EntityInfo {
            owner: Some(player),
            ..info(EntityKind::Ped, 0.0)
        }
    }

    fn dirty(store: &mut EntityStore) -> Vec<EntityUpdate> {
        let mut out = Vec::new();
        store.take_dirty(&mut out);
        out
    }

    /// The position of an owner move, and the rotation it keeps.
    const OWNER_POSITION: [f32; 3] = [9.0, 8.0, 7.0];

    #[test]
    fn ids_count_up_from_one_and_are_never_reused() {
        let mut store = EntityStore::default();
        assert_eq!(store.insert(info(EntityKind::Vehicle, 0.0)).unwrap(), 1);
        assert_eq!(store.insert(info(EntityKind::Vehicle, 0.0)).unwrap(), 2);
        assert!(store.remove(1).is_some());
        assert_eq!(store.insert(info(EntityKind::Vehicle, 0.0)).unwrap(), 3);
    }

    #[test]
    fn ids_run_out_after_the_last_one() {
        let mut store = EntityStore {
            next_id: u32::MAX,
            ..EntityStore::default()
        };
        assert_eq!(
            store.insert(info(EntityKind::Object, 0.0)).unwrap(),
            u32::MAX
        );
        assert!(store.insert(info(EntityKind::Object, 0.0)).is_err());
    }

    #[test]
    fn no_more_than_max_entities_can_exist() {
        let mut store = EntityStore::default();
        for _ in 0..MAX_ENTITIES {
            store.insert(info(EntityKind::Object, 0.0)).unwrap();
        }
        assert!(store.insert(info(EntityKind::Object, 0.0)).is_err());
        store.remove(1);
        assert_eq!(
            store.insert(info(EntityKind::Object, 0.0)).unwrap(),
            u32::try_from(MAX_ENTITIES + 1).unwrap()
        );
    }

    #[test]
    fn non_finite_transforms_are_rejected_and_change_nothing() {
        let mut store = EntityStore::default();
        let mut bad = info(EntityKind::Ped, 0.0);
        bad.position = [f32::NAN, 0.0, 0.0];
        assert!(store.insert(bad).is_err());
        assert_eq!(store.all().len(), 0);

        let id = store.insert(info(EntityKind::Ped, 1.0)).unwrap();
        assert!(store.set_position(id, [f32::INFINITY, 0.0, 0.0]).is_err());
        assert!(store.set_rotation(id, [0.0, f32::NAN, 0.0]).is_err());
        assert_eq!(store.get(id).unwrap().position, [1.0, 0.0, 0.0]);
        assert_eq!(dirty(&mut store).len(), 0);
    }

    #[test]
    fn two_moves_in_one_frame_send_one_update_with_the_last_transform() {
        let mut store = EntityStore::default();
        let id = store.insert(info(EntityKind::Vehicle, 0.0)).unwrap();
        store.set_position(id, [1.0, 0.0, 0.0]).unwrap();
        store.set_position(id, [2.0, 0.0, 0.0]).unwrap();
        store.set_rotation(id, [0.0, 0.0, 90.0]).unwrap();

        assert_eq!(
            dirty(&mut store),
            vec![(id, [2.0, 0.0, 0.0], [0.0, 0.0, 90.0], 3)],
            "three server moves, one update carrying the last transform and epoch"
        );
        assert!(dirty(&mut store).is_empty(), "the queue is cleared");
    }

    #[test]
    fn a_deleted_entity_drops_out_of_the_dirty_list() {
        let mut store = EntityStore::default();
        let gone = store.insert(info(EntityKind::Vehicle, 0.0)).unwrap();
        let kept = store.insert(info(EntityKind::Vehicle, 0.0)).unwrap();
        store.set_position(gone, [5.0, 0.0, 0.0]).unwrap();
        store.set_position(kept, [6.0, 0.0, 0.0]).unwrap();
        store.remove(gone);

        assert_eq!(
            dirty(&mut store),
            vec![(kept, [6.0, 0.0, 0.0], [0.0, 0.0, 0.0], 1)]
        );
    }

    #[test]
    fn moving_an_unknown_entity_fails() {
        let mut store = EntityStore::default();
        assert!(store.set_position(7, [0.0; 3]).is_err());
        assert!(store.set_rotation(7, [0.0; 3]).is_err());
    }

    #[test]
    fn a_server_move_advances_the_epoch_and_an_owner_move_keeps_it() {
        let mut store = EntityStore::default();
        let ped = store.insert(owned_by(4)).unwrap();
        assert!(store.owner_move(4, ped, OWNER_POSITION, [0.0; 3], 0));
        assert_eq!(
            dirty(&mut store),
            vec![(ped, OWNER_POSITION, [0.0; 3], 0)],
            "an owner move keeps epoch 0"
        );

        store.set_position(ped, [1.0, 1.0, 1.0]).unwrap();
        assert_eq!(
            dirty(&mut store),
            vec![(ped, [1.0, 1.0, 1.0], [0.0; 3], 1)],
            "a server move starts epoch 1"
        );
        assert!(store.owner_move(4, ped, OWNER_POSITION, [0.0; 3], 1));
        assert_eq!(
            dirty(&mut store),
            vec![(ped, OWNER_POSITION, [0.0; 3], 1)],
            "an owner move at the current epoch keeps it"
        );
    }

    #[test]
    fn an_owner_update_for_an_old_epoch_is_refused_after_a_server_move_in_the_same_frame() {
        let mut store = EntityStore::default();
        let ped = store.insert(owned_by(4)).unwrap();
        store.set_position(ped, [100.0, 0.0, 0.0]).unwrap();
        // The owner's update was computed before the teleport, so it is refused
        // whatever the order of arrival, and the teleport keeps its position.
        assert!(!store.owner_move(4, ped, OWNER_POSITION, [0.0; 3], 0));
        assert_eq!(
            dirty(&mut store),
            vec![(ped, [100.0, 0.0, 0.0], [0.0; 3], 1)]
        );
    }

    #[test]
    fn an_owner_update_for_an_old_epoch_is_refused_after_the_server_move_was_sent() {
        let mut store = EntityStore::default();
        let ped = store.insert(owned_by(4)).unwrap();
        store.set_position(ped, [100.0, 0.0, 0.0]).unwrap();
        dirty(&mut store);

        // A stale update that arrives after the teleport frame is still refused,
        // and the owner's update made with the new epoch is accepted.
        assert!(!store.owner_move(4, ped, OWNER_POSITION, [0.0; 3], 0));
        assert_eq!(store.get(ped).unwrap().position, [100.0, 0.0, 0.0]);
        assert!(store.owner_move(4, ped, OWNER_POSITION, [0.0; 3], 1));
        assert_eq!(store.get(ped).unwrap().position, OWNER_POSITION);
        assert_eq!(dirty(&mut store), vec![(ped, OWNER_POSITION, [0.0; 3], 1)]);
    }

    #[test]
    fn the_epoch_wraps_around() {
        let mut store = EntityStore::default();
        let ped = store.insert(owned_by(4)).unwrap();
        store.by_id.get_mut(&ped).unwrap().epoch = u32::MAX;
        store.set_position(ped, [1.0, 0.0, 0.0]).unwrap();
        assert_eq!(dirty(&mut store), vec![(ped, [1.0, 0.0, 0.0], [0.0; 3], 0)]);
    }

    #[test]
    fn owner_moves_are_refused_for_the_wrong_player_or_entity() {
        let mut store = EntityStore::default();
        let ped = store.insert(owned_by(4)).unwrap();
        let vehicle = store.insert(info(EntityKind::Vehicle, 0.0)).unwrap();

        assert!(
            !store.owner_move(5, ped, OWNER_POSITION, [0.0; 3], 0),
            "not the owner"
        );
        assert!(
            !store.owner_move(4, vehicle, OWNER_POSITION, [0.0; 3], 0),
            "script-created"
        );
        assert!(
            !store.owner_move(4, 99, OWNER_POSITION, [0.0; 3], 0),
            "unknown entity"
        );
        assert!(dirty(&mut store).is_empty(), "nothing is queued");
        assert_eq!(store.get(ped).unwrap().position, [0.0; 3]);
    }

    #[test]
    fn owner_moves_with_a_value_that_is_not_finite_are_refused() {
        let mut store = EntityStore::default();
        let ped = store.insert(owned_by(4)).unwrap();
        assert!(!store.owner_move(4, ped, [f32::NAN, 0.0, 0.0], [0.0; 3], 0));
        assert!(!store.owner_move(4, ped, [0.0; 3], [0.0, f32::INFINITY, 0.0], 0));
        assert!(dirty(&mut store).is_empty(), "nothing is queued");
    }

    #[test]
    fn entities_are_listed_by_kind_in_id_order() {
        let mut store = EntityStore::default();
        let vehicle_a = store.insert(info(EntityKind::Vehicle, 0.0)).unwrap();
        let object = store.insert(info(EntityKind::Object, 0.0)).unwrap();
        let ped = store.insert(info(EntityKind::Ped, 0.0)).unwrap();
        let vehicle_b = store.insert(info(EntityKind::Vehicle, 0.0)).unwrap();

        assert_eq!(
            store.ids(Some(EntityKind::Vehicle)),
            vec![vehicle_a, vehicle_b]
        );
        assert_eq!(store.ids(Some(EntityKind::Object)), vec![object]);
        assert_eq!(store.ids(Some(EntityKind::Ped)), vec![ped]);
        assert_eq!(
            store.ids(None),
            vec![vehicle_a, object, ped, vehicle_b],
            "every kind, ascending"
        );
        assert!(store.get(object).is_some());
        assert!(!store.contains(99));
    }
}
