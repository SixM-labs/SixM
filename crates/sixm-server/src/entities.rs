//! Entities: their ids, their transforms, and the transform updates due for
//! clients at the end of each frame.
//!
//! Scripts create, move and delete entities. Creation and deletion reach clients
//! at once, and with distance scoping (see the `scope` module) only the clients
//! that hold the entity. A move only marks the entity, and each marked entity is
//! sent once per frame with its latest transform. The owner of an entity may
//! move it too, and then the other clients receive the owner's transform.

use std::collections::{BTreeMap, HashSet, VecDeque};

use sixm_core::{
    EntityAttachment, EntityInfo, EntityKind, EntityState, EventSource, MAX_ENTITIES, ScriptError,
    Value, entity_bag, is_finite, joaat,
};
use sixm_net::{
    EntityAttachmentEntry, EntityStateUpdate, EntityTransform, STATE_ALPHA, STATE_ATTACHMENT,
    STATE_FROZEN, STATE_HEALTH, STATE_MAX_HEALTH, STATE_VELOCITY, STATE_VISIBLE,
};
use tracing::{debug, warn};

use crate::manager::ResourceManager;
use crate::scope::{Grid, squared_distance};

/// One transform as clients receive it: the entity id, its position and its
/// rotation, each present only when it changed since the last frame, and its
/// teleport epoch.
pub(crate) type EntityUpdate = EntityTransform;

/// An entity, its teleport epoch, and which of its parts changed since the
/// last frame.
#[derive(Debug, Clone)]
struct Slot {
    info: EntityInfo,
    state: EntityState,
    /// How many times the server has moved the entity itself. Only the server
    /// changes it, and an owner's update must carry the current value.
    epoch: u32,
    /// The position changed since the last frame.
    position_dirty: bool,
    /// The rotation changed since the last frame.
    rotation_dirty: bool,
    /// Bitmask of `EntityState` fields that changed since the last frame.
    state_dirty: u8,
    /// The client that asked for the entity, or `None` for one a script made.
    creator: Option<u32>,
    /// Whether the entity is handed to another player when its creator leaves,
    /// rather than deleted.
    migrate_on_orphan: bool,
    /// Resource name that created the entity, if created by a server script.
    script: Option<String>,
    /// Optional per-entity culling radius override in metres.
    cull_radius: Option<f32>,
}

impl Slot {
    /// Whether a part changed since the last frame, so the entity is queued.
    fn is_dirty(&self) -> bool {
        self.position_dirty || self.rotation_dirty
    }

    /// Takes the parts that changed since the last frame, clearing their flags.
    fn take_update(&mut self, id: u32) -> EntityUpdate {
        let position = self.position_dirty.then_some(self.info.position);
        let rotation = self.rotation_dirty.then_some(self.info.rotation);
        self.position_dirty = false;
        self.rotation_dirty = false;
        (id, position, rotation, self.epoch)
    }

    /// Takes the state fields that changed since the last frame, clearing
    /// their bitmask. Returns `None` if nothing changed.
    fn take_state_update(&mut self, id: u32) -> Option<EntityStateUpdate> {
        let flags = self.state_dirty;
        if flags == 0 {
            return None;
        }
        self.state_dirty = 0;
        Some(EntityStateUpdate {
            id,
            frozen: (flags & STATE_FROZEN != 0).then_some(self.state.frozen),
            velocity: (flags & STATE_VELOCITY != 0).then_some(self.state.velocity),
            health: (flags & STATE_HEALTH != 0).then_some(self.state.health),
            max_health: (flags & STATE_MAX_HEALTH != 0).then_some(self.state.max_health),
            visible: (flags & STATE_VISIBLE != 0).then_some(self.state.visible),
            alpha: (flags & STATE_ALPHA != 0).then_some(self.state.alpha),
            attachment: (flags & STATE_ATTACHMENT != 0)
                .then_some(self.state.attachment.map(EntityAttachmentEntry::from)),
        })
    }
}

/// Every entity on the server, by id, and the ids whose transform or state
/// changed since the last frame.
#[derive(Debug)]
pub(crate) struct EntityStore {
    /// The next id to hand out. Zero once every id has been used.
    next_id: u32,
    by_id: BTreeMap<u32, Slot>,
    /// Ids queued for transform updates at the next frame, each listed once.
    dirty: Vec<u32>,
    /// Ids queued for state updates at the next frame, each listed once.
    state_dirty: Vec<u32>,
    /// Attached children per parent entity id.
    children: BTreeMap<u32, Vec<u32>>,
    /// The ids of the entities each player owns, in creation order.
    owned: BTreeMap<u32, Vec<u32>>,
    /// The entities by position, for finding those near a player. Only kept
    /// while scoping is on.
    grid: Option<Grid>,
    /// Players whose owned list or owned entity bucket changed since the last
    /// scope refresh. Only tracked while the grid is active.
    dirty_owners: HashSet<u32>,
    /// The routing bucket of each entity outside bucket 0. Empty while every
    /// entity is in bucket 0, so the bucket checks cost nothing then.
    buckets: BTreeMap<u32, u32>,
    /// The ids of the entities each client created, in creation order.
    created: BTreeMap<u32, Vec<u32>>,
    /// The id blocks each client may still create entities with, in the order
    /// it uses them. A block is `(next, end)`: the ids from `next` up to `end`,
    /// `end` not included, are not yet used.
    leases: BTreeMap<u32, VecDeque<(u32, u32)>>,
    /// Entities with an explicit `SetEntityDistanceCullingRadius` override.
    custom_cull: HashSet<u32>,
    /// Set when any entity's custom cull radius changes since the last scope refresh.
    custom_cull_dirty: bool,
}

impl Default for EntityStore {
    fn default() -> Self {
        Self {
            next_id: 1,
            by_id: BTreeMap::new(),
            dirty: Vec::new(),
            state_dirty: Vec::new(),
            children: BTreeMap::new(),
            owned: BTreeMap::new(),
            grid: None,
            dirty_owners: HashSet::new(),
            buckets: BTreeMap::new(),
            created: BTreeMap::new(),
            leases: BTreeMap::new(),
            custom_cull: HashSet::new(),
            custom_cull_dirty: false,
        }
    }
}

impl EntityStore {
    /// Turns the position index on, with cells `size` metres wide, or off with
    /// `None`. A new index covers every entity at its current position.
    pub(crate) fn set_grid(&mut self, size: Option<f32>) {
        self.dirty_owners.clear();
        self.grid = size.map(|size| {
            let mut grid = Grid::new(size);
            for (id, slot) in &self.by_id {
                grid.insert(*id, slot.info.position);
            }
            grid
        });
    }

    /// The ids of the entities `player` owns, in creation order.
    pub(crate) fn owned(&self, player: u32) -> &[u32] {
        self.owned.get(&player).map_or(&[], Vec::as_slice)
    }

    /// The position of the entity `id`, if it exists.
    pub(crate) fn position(&self, id: u32) -> Option<[f32; 3]> {
        self.by_id.get(&id).map(|slot| slot.info.position)
    }

    /// The entity `id` with its teleport epoch, if it exists.
    pub(crate) fn info_and_epoch(&self, id: u32) -> Option<(EntityInfo, u32)> {
        self.by_id.get(&id).map(|slot| (slot.info, slot.epoch))
    }

    /// The entity `id` with its state and teleport epoch, if it exists.
    pub(crate) fn info_state_and_epoch(&self, id: u32) -> Option<(EntityInfo, EntityState, u32)> {
        self.by_id
            .get(&id)
            .map(|slot| (slot.info, slot.state, slot.epoch))
    }

    /// The replicated state of the entity `id`, if it exists.
    pub(crate) fn state(&self, id: u32) -> Option<EntityState> {
        self.by_id.get(&id).map(|slot| slot.state)
    }

    /// Every entity id, in ascending order.
    pub(crate) fn ids_ascending(&self) -> impl Iterator<Item = u32> + '_ {
        self.by_id.keys().copied()
    }

    /// The routing bucket of the entity `id`. Zero for an unknown entity.
    pub(crate) fn bucket(&self, id: u32) -> u32 {
        self.buckets.get(&id).copied().unwrap_or(0)
    }

    /// Whether the entity `id` is in `bucket`. Costs nothing when every entity
    /// is in bucket 0 and `bucket` is 0.
    pub(crate) fn in_bucket(&self, id: u32, bucket: u32) -> bool {
        (bucket == 0 && self.buckets.is_empty()) || self.bucket(id) == bucket
    }

    /// Whether any entity is outside bucket 0.
    pub(crate) fn any_bucket(&self) -> bool {
        !self.buckets.is_empty()
    }

    /// Moves the entity `id` to `bucket`. Does nothing for an unknown entity.
    pub(crate) fn set_bucket(&mut self, id: u32, bucket: u32) {
        let Some(slot) = self.by_id.get(&id) else {
            return;
        };
        if self.bucket(id) == bucket {
            return;
        }
        if self.grid.is_some()
            && let Some(owner) = slot.info.owner
        {
            self.dirty_owners.insert(owner);
        }
        if let Some(grid) = self.grid.as_mut()
            && let Some(cell) = grid.cell_of_entity(id)
        {
            grid.mark_dirty(cell);
        }
        if bucket == 0 {
            self.buckets.remove(&id);
        } else {
            self.buckets.insert(id, bucket);
        }
    }

    /// Whether any player's owned entity set or owned entity bucket changed
    /// since the last scope refresh.
    pub(crate) fn has_dirty_owners(&self) -> bool {
        !self.dirty_owners.is_empty()
    }

    /// Whether `player`'s owned entity set or an owned entity's bucket changed
    /// since the last scope refresh.
    pub(crate) fn is_owner_dirty(&self, player: u32) -> bool {
        self.dirty_owners.contains(&player)
    }

    /// Whether any entity entered, left, moved, or changed bucket in the nine
    /// cells around `position` since the last scope refresh.
    pub(crate) fn any_dirty_near(&self, position: [f32; 3]) -> bool {
        self.grid
            .as_ref()
            .is_some_and(|grid| grid.any_dirty_near(position))
    }

    /// Clears the scope dirty flags after a frame's scope refresh.
    pub(crate) fn clear_scope_dirty(&mut self) {
        self.dirty_owners.clear();
        self.custom_cull_dirty = false;
        if let Some(grid) = self.grid.as_mut() {
            grid.clear_dirty();
        }
    }

    /// Fills `out` with the scope of `player` in `bucket`: the entities within
    /// `radius` metres of `center` (or their custom culling radius), and the
    /// entities it owns, that are in the bucket. The list is in ascending order
    /// and has no duplicates. Without a center, only the owned entities are listed.
    pub(crate) fn scope_into(
        &self,
        player: u32,
        center: Option<[f32; 3]>,
        radius: f32,
        bucket: u32,
        out: &mut Vec<u32>,
    ) {
        out.clear();
        if let (Some(center), Some(grid)) = (center, self.grid.as_ref()) {
            let default_limit = radius * radius;
            let has_custom = !self.custom_cull.is_empty();
            grid.for_each_near(center, |id, position| {
                let limit = if has_custom {
                    self.cull_radius(id).map_or(default_limit, |r| r * r)
                } else {
                    default_limit
                };
                if squared_distance(position, center) <= limit && self.in_bucket(id, bucket) {
                    out.push(id);
                }
            });
            if has_custom {
                for &id in &self.custom_cull {
                    if let Some(custom_radius) = self.cull_radius(id)
                        && custom_radius > radius
                        && let Some(position) = self.position(id)
                        && squared_distance(position, center) <= custom_radius * custom_radius
                        && self.in_bucket(id, bucket)
                    {
                        out.push(id);
                    }
                }
            }
        }
        out.extend(
            self.owned(player)
                .iter()
                .copied()
                .filter(|&id| self.in_bucket(id, bucket)),
        );
        out.sort_unstable();
        out.dedup();
    }

    /// Stores `info` as a new entity made by a script and returns its id. Fails
    /// for a transform that is not finite, when [`MAX_ENTITIES`] entities exist,
    /// or when no ids are left.
    pub(crate) fn insert(&mut self, info: EntityInfo) -> Result<u32, ScriptError> {
        self.check_room(&info)?;
        let id = self.next_id;
        if id == 0 {
            return Err(ScriptError::Runtime("no entity ids are left".to_owned()));
        }
        self.next_id = id.checked_add(1).unwrap_or(0);
        self.place(id, info, None, false);
        Ok(id)
    }

    /// Stores `info` as the entity `id`, which a client created with an id
    /// leased to it. The client is its creator, and `migrate_on_orphan` says
    /// whether it is handed on when the creator leaves. Fails like
    /// [`insert`](Self::insert), and when `id` is already an entity.
    pub(crate) fn insert_created(
        &mut self,
        id: u32,
        info: EntityInfo,
        creator: u32,
    ) -> Result<(), ScriptError> {
        self.check_room(&info)?;
        if self.by_id.contains_key(&id) {
            return Err(ScriptError::Runtime(format!("entity {id} already exists")));
        }
        self.place(id, info, Some(creator), false);
        Ok(())
    }

    /// Checks a new entity's transform and the room left for it.
    fn check_room(&self, info: &EntityInfo) -> Result<(), ScriptError> {
        check_finite(info.position)?;
        check_finite(info.rotation)?;
        if self.by_id.len() >= MAX_ENTITIES {
            return Err(ScriptError::Runtime(format!(
                "at most {MAX_ENTITIES} entities can exist"
            )));
        }
        Ok(())
    }

    /// Stores a checked entity under `id` and indexes it.
    fn place(&mut self, id: u32, info: EntityInfo, creator: Option<u32>, migrate_on_orphan: bool) {
        self.by_id.insert(
            id,
            Slot {
                info,
                state: EntityState::default(),
                epoch: 0,
                position_dirty: false,
                rotation_dirty: false,
                state_dirty: 0,
                creator,
                migrate_on_orphan,
                script: None,
                cull_radius: None,
            },
        );
        if let Some(grid) = self.grid.as_mut() {
            grid.insert(id, info.position);
            if let Some(owner) = info.owner {
                self.dirty_owners.insert(owner);
            }
        }
        if let Some(owner) = info.owner {
            self.owned.entry(owner).or_default().push(id);
        }
        if let Some(creator) = creator {
            self.created.entry(creator).or_default().push(id);
        }
    }

    /// Removes the entity `id`, and returns it when it existed. Any children
    /// attached to `id` are automatically detached and queued for state sync.
    pub(crate) fn remove(&mut self, id: u32) -> Option<EntityInfo> {
        let slot = self.by_id.remove(&id)?;
        let info = slot.info;
        self.buckets.remove(&id);
        self.custom_cull.remove(&id);
        let has_grid = self.grid.is_some();
        if let Some(grid) = self.grid.as_mut() {
            grid.remove(id);
        }
        if let Some(owner) = info.owner {
            self.disown(owner, id);
            if has_grid {
                self.dirty_owners.insert(owner);
            }
        }
        if let Some(creator) = slot.creator {
            self.uncreate(creator, id);
        }
        if let Some(att) = slot.state.attachment {
            self.detach_child_index(att.parent, id);
        }
        if let Some(child_ids) = self.children.remove(&id) {
            for child_id in child_ids {
                if let Some(child) = self.by_id.get_mut(&child_id)
                    && child.state.attachment.is_some()
                {
                    child.state.attachment = None;
                    let queued = child.state_dirty != 0;
                    child.state_dirty |= STATE_ATTACHMENT;
                    if !queued {
                        self.state_dirty.push(child_id);
                    }
                }
            }
        }
        Some(info)
    }

    /// Removes `child_id` from `parent_id`'s children list.
    fn detach_child_index(&mut self, parent_id: u32, child_id: u32) {
        if let Some(list) = self.children.get_mut(&parent_id) {
            list.retain(|&id| id != child_id);
            if list.is_empty() {
                self.children.remove(&parent_id);
            }
        }
    }

    /// Drops `id` from the entities `creator` created.
    fn uncreate(&mut self, creator: u32, id: u32) {
        if let Some(ids) = self.created.get_mut(&creator) {
            ids.retain(|&made| made != id);
            if ids.is_empty() {
                self.created.remove(&creator);
            }
        }
    }

    /// The ids of the entities `creator` created, in creation order.
    pub(crate) fn created_by(&self, creator: u32) -> &[u32] {
        self.created.get(&creator).map_or(&[], Vec::as_slice)
    }

    /// Original creator of `id`, if it was created by a client (retained even after owner transfer).
    pub(crate) fn creator(&self, id: u32) -> Option<u32> {
        self.by_id.get(&id).and_then(|slot| slot.creator)
    }

    /// Records the resource name that created `id`.
    pub(crate) fn set_script(&mut self, id: u32, script: &str) {
        if let Some(slot) = self.by_id.get_mut(&id) {
            slot.script = Some(script.to_owned());
        }
    }

    /// Name of the resource that created `id`, if known.
    pub(crate) fn script(&self, id: u32) -> Option<String> {
        self.by_id.get(&id).and_then(|slot| slot.script.clone())
    }

    /// Per-entity culling radius override, if set.
    pub(crate) fn cull_radius(&self, id: u32) -> Option<f32> {
        self.by_id.get(&id).and_then(|slot| slot.cull_radius)
    }

    /// Sets per-entity distance culling radius (`<= 0` resets to global).
    pub(crate) fn set_cull_radius(&mut self, id: u32, radius: f32) -> Result<(), ScriptError> {
        if !radius.is_finite() {
            return Err(ScriptError::Runtime(
                "distance culling radius must be finite".to_owned(),
            ));
        }
        let slot = self
            .by_id
            .get_mut(&id)
            .ok_or_else(|| ScriptError::Runtime(format!("entity {id} does not exist")))?;
        let next = (radius > 0.0).then_some(radius);
        if slot.cull_radius.map(f32::to_bits) != next.map(f32::to_bits) {
            slot.cull_radius = next;
            if next.is_some() {
                self.custom_cull.insert(id);
            } else {
                self.custom_cull.remove(&id);
            }
            self.custom_cull_dirty = true;
        }
        Ok(())
    }

    /// Whether any custom per-entity cull radius changed since the last scope refresh.
    pub(crate) fn has_custom_cull_dirty(&self) -> bool {
        self.custom_cull_dirty
    }

    /// Whether the entity `id` is handed on when its creator leaves.
    pub(crate) fn migrates_on_orphan(&self, id: u32) -> bool {
        self.by_id
            .get(&id)
            .is_some_and(|slot| slot.migrate_on_orphan)
    }

    /// Sets whether the entity `id` is handed on when its creator leaves.
    /// Does nothing for an unknown entity.
    pub(crate) fn set_migrates_on_orphan(&mut self, id: u32, migrate: bool) {
        if let Some(slot) = self.by_id.get_mut(&id) {
            slot.migrate_on_orphan = migrate;
        }
    }

    /// Forgets the creator of the entity `id`, once that client has left. The
    /// entity stays. Does nothing for an unknown entity.
    pub(crate) fn release_creator(&mut self, id: u32) {
        let Some(slot) = self.by_id.get_mut(&id) else {
            return;
        };
        if let Some(creator) = slot.creator.take() {
            self.uncreate(creator, id);
        }
    }

    /// Gives the entity `id` to `owner`, or to the server with `None`. The
    /// entity keeps its teleport epoch, so the new owner starts from the
    /// current one. Returns `false` when the entity is unknown or already has
    /// that owner.
    pub(crate) fn set_owner(&mut self, id: u32, owner: Option<u32>) -> bool {
        let Some(slot) = self.by_id.get_mut(&id) else {
            return false;
        };
        let previous = std::mem::replace(&mut slot.info.owner, owner);
        if previous == owner {
            return false;
        }
        if let Some(previous) = previous {
            self.disown(previous, id);
            if self.grid.is_some() {
                self.dirty_owners.insert(previous);
            }
        }
        if let Some(owner) = owner {
            self.owned.entry(owner).or_default().push(id);
            if self.grid.is_some() {
                self.dirty_owners.insert(owner);
            }
        }
        true
    }

    /// Leases `count` ids to `player`: the ids from the next unused one on, and
    /// they are never handed to anything else. Returns the first id and the
    /// count, or `None` when the ids would run out.
    pub(crate) fn lease_ids(&mut self, player: u32, count: u32) -> Option<(u32, u32)> {
        let first = self.next_id;
        let end = first.checked_add(count).filter(|_| first != 0)?;
        self.next_id = end;
        self.leases
            .entry(player)
            .or_default()
            .push_back((first, end));
        Some((first, count))
    }

    /// The number of ids `player` may still create entities with.
    pub(crate) fn leased_remaining(&self, player: u32) -> u32 {
        self.leases.get(&player).map_or(0, |blocks| {
            blocks
                .iter()
                .fold(0, |sum, &(next, end)| sum + (end - next))
        })
    }

    /// Uses the leased id `id` of `player`. It must be the next id of the
    /// player's first block, which is the order the client takes them in.
    /// Returns `false` and changes nothing otherwise.
    pub(crate) fn use_leased_id(&mut self, player: u32, id: u32) -> bool {
        let Some(blocks) = self.leases.get_mut(&player) else {
            return false;
        };
        let Some(front) = blocks.front_mut() else {
            return false;
        };
        if front.0 != id {
            return false;
        }
        front.0 += 1;
        if front.0 == front.1 {
            blocks.pop_front();
        }
        if blocks.is_empty() {
            self.leases.remove(&player);
        }
        true
    }

    /// Forgets every id block of `player`, once it has left.
    pub(crate) fn drop_leases(&mut self, player: u32) {
        self.leases.remove(&player);
    }

    /// Drops `id` from the entities `owner` owns.
    fn disown(&mut self, owner: u32, id: u32) {
        if let Some(ids) = self.owned.get_mut(&owner) {
            ids.retain(|&owned| owned != id);
            if ids.is_empty() {
                self.owned.remove(&owner);
            }
        }
    }

    /// The entity `id`, if it exists.
    pub(crate) fn get(&self, id: u32) -> Option<EntityInfo> {
        self.by_id.get(&id).map(|slot| slot.info)
    }

    /// Whether the entity `id` exists.
    pub(crate) fn contains(&self, id: u32) -> bool {
        self.by_id.contains_key(&id)
    }

    /// Every entity with its state and epoch, in ascending id order.
    pub(crate) fn all(&self) -> Vec<(u32, EntityInfo, EntityState, u32)> {
        self.by_id
            .iter()
            .map(|(id, slot)| (*id, slot.info, slot.state, slot.epoch))
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
        self.update(id, Some(position), None)
    }

    /// Rotates the entity `id` on the server, and queues it for the next frame
    /// with a new epoch.
    pub(crate) fn set_rotation(&mut self, id: u32, rotation: [f32; 3]) -> Result<(), ScriptError> {
        check_finite(rotation)?;
        self.update(id, None, Some(rotation))
    }

    /// Applies a server move to the entity `id`: the parts given change, the
    /// epoch goes up, so every owner update sent before it is refused, and the
    /// entity is queued.
    fn update(
        &mut self,
        id: u32,
        position: Option<[f32; 3]>,
        rotation: Option<[f32; 3]>,
    ) -> Result<(), ScriptError> {
        let slot = self
            .by_id
            .get_mut(&id)
            .ok_or_else(|| ScriptError::Runtime(format!("entity {id} does not exist")))?;
        let queued = slot.is_dirty();
        if let Some(position) = position {
            slot.info.position = position;
            slot.position_dirty = true;
        }
        if let Some(rotation) = rotation {
            slot.info.rotation = rotation;
            slot.rotation_dirty = true;
        }
        slot.epoch = slot.epoch.wrapping_add(1);
        if let (Some(grid), Some(position)) = (self.grid.as_mut(), position) {
            grid.move_to(id, position);
        }
        if !queued {
            self.dirty.push(id);
        }
        self.propagate_attachment_transforms(id);
        Ok(())
    }

    /// Stores a transform that `player`, the owner of the entity `id`, sent for
    /// the teleport epoch `epoch`. Each part given is stored, and a part that is
    /// `None` keeps its value. Returns `false` and changes nothing unless the
    /// entity exists, `player` owns it, it is neither frozen nor attached, at
    /// least one part is given, every part given is finite, and `epoch` is the
    /// entity's current epoch. An update computed before a server move therefore
    /// never lands, whenever it arrives. An accepted transform keeps the epoch,
    /// and updates any attached children.
    pub(crate) fn owner_move(
        &mut self,
        player: u32,
        id: u32,
        position: Option<[f32; 3]>,
        rotation: Option<[f32; 3]>,
        epoch: u32,
    ) -> bool {
        let Some(slot) = self.by_id.get_mut(&id) else {
            return false;
        };
        if slot.info.owner != Some(player)
            || slot.state.frozen
            || slot.state.attachment.is_some()
            || slot.epoch != epoch
            || (position.is_none() && rotation.is_none())
            || !position.is_none_or(is_finite)
            || !rotation.is_none_or(is_finite)
        {
            return false;
        }
        let queued = slot.is_dirty();
        if let Some(position) = position {
            slot.info.position = position;
            slot.position_dirty = true;
            if let Some(grid) = self.grid.as_mut() {
                grid.move_to(id, position);
            }
        }
        if let Some(rotation) = rotation {
            slot.info.rotation = rotation;
            slot.rotation_dirty = true;
        }
        if !queued {
            self.dirty.push(id);
        }
        self.propagate_attachment_transforms(id);
        true
    }

    /// Moves the parts that changed of every queued entity into `out`, in the
    /// order the entities were first moved, and clears the queue. The entries
    /// already in `out` are kept.
    pub(crate) fn take_dirty(&mut self, out: &mut Vec<EntityUpdate>) {
        for id in self.dirty.drain(..) {
            if let Some(slot) = self.by_id.get_mut(&id) {
                out.push(slot.take_update(id));
            }
        }
    }

    /// Moves the state changes of every queued entity into `out`, in the order
    /// they were first dirtied, and clears the state queue.
    pub(crate) fn take_state_dirty(&mut self, out: &mut Vec<EntityStateUpdate>) {
        for id in self.state_dirty.drain(..) {
            if let Some(slot) = self.by_id.get_mut(&id)
                && let Some(update) = slot.take_state_update(id)
            {
                out.push(update);
            }
        }
    }

    /// Marks `bit` dirty on `id` and queues it if not already queued.
    fn mark_state_dirty(&mut self, id: u32, bit: u8) {
        if let Some(slot) = self.by_id.get_mut(&id) {
            let queued = slot.state_dirty != 0;
            slot.state_dirty |= bit;
            if !queued {
                self.state_dirty.push(id);
            }
        }
    }

    /// Freezes or unfreezes entity `id`.
    pub(crate) fn set_frozen(&mut self, id: u32, frozen: bool) -> Result<(), ScriptError> {
        let slot = self
            .by_id
            .get_mut(&id)
            .ok_or_else(|| ScriptError::Runtime(format!("entity {id} does not exist")))?;
        if slot.state.frozen != frozen {
            slot.state.frozen = frozen;
            self.mark_state_dirty(id, STATE_FROZEN);
        }
        Ok(())
    }

    /// Sets entity `id`'s velocity.
    pub(crate) fn set_velocity(&mut self, id: u32, velocity: [f32; 3]) -> Result<(), ScriptError> {
        check_finite(velocity)?;
        let slot = self
            .by_id
            .get_mut(&id)
            .ok_or_else(|| ScriptError::Runtime(format!("entity {id} does not exist")))?;
        let changed = slot.state.velocity[0].to_bits() != velocity[0].to_bits()
            || slot.state.velocity[1].to_bits() != velocity[1].to_bits()
            || slot.state.velocity[2].to_bits() != velocity[2].to_bits();
        if changed {
            slot.state.velocity = velocity;
            self.mark_state_dirty(id, STATE_VELOCITY);
        }
        Ok(())
    }

    /// Sets entity `id`'s health (clamped to `0..=i32::MAX`).
    pub(crate) fn set_health(&mut self, id: u32, health: i32) -> Result<(), ScriptError> {
        let health = health.max(0);
        let slot = self
            .by_id
            .get_mut(&id)
            .ok_or_else(|| ScriptError::Runtime(format!("entity {id} does not exist")))?;
        if slot.state.health != health {
            slot.state.health = health;
            self.mark_state_dirty(id, STATE_HEALTH);
        }
        Ok(())
    }

    /// Sets entity `id`'s maximum health (clamped to `0..=i32::MAX`).
    pub(crate) fn set_max_health(&mut self, id: u32, max_health: i32) -> Result<(), ScriptError> {
        let max_health = max_health.max(0);
        let slot = self
            .by_id
            .get_mut(&id)
            .ok_or_else(|| ScriptError::Runtime(format!("entity {id} does not exist")))?;
        if slot.state.max_health != max_health {
            slot.state.max_health = max_health;
            self.mark_state_dirty(id, STATE_MAX_HEALTH);
        }
        Ok(())
    }

    /// Sets whether entity `id` is visible.
    pub(crate) fn set_visible(&mut self, id: u32, visible: bool) -> Result<(), ScriptError> {
        let slot = self
            .by_id
            .get_mut(&id)
            .ok_or_else(|| ScriptError::Runtime(format!("entity {id} does not exist")))?;
        if slot.state.visible != visible {
            slot.state.visible = visible;
            self.mark_state_dirty(id, STATE_VISIBLE);
        }
        Ok(())
    }

    /// Sets entity `id`'s alpha (`0..=255`).
    pub(crate) fn set_alpha(&mut self, id: u32, alpha: u8) -> Result<(), ScriptError> {
        let slot = self
            .by_id
            .get_mut(&id)
            .ok_or_else(|| ScriptError::Runtime(format!("entity {id} does not exist")))?;
        if slot.state.alpha != alpha {
            slot.state.alpha = alpha;
            self.mark_state_dirty(id, STATE_ALPHA);
        }
        Ok(())
    }

    /// Attaches `id` to `attachment.parent`. Prevents self-attachment and
    /// cycles, updates `id`'s world transform (and its children's) to follow
    /// the parent, and marks state + transform dirty.
    pub(crate) fn attach(
        &mut self,
        id: u32,
        attachment: EntityAttachment,
    ) -> Result<(), ScriptError> {
        check_finite(attachment.offset)?;
        check_finite(attachment.rotation)?;
        if !self.by_id.contains_key(&id) {
            return Err(ScriptError::Runtime(format!("entity {id} does not exist")));
        }
        let target = attachment.parent;
        if !self.by_id.contains_key(&target) {
            return Err(ScriptError::Runtime(format!(
                "target entity {target} does not exist"
            )));
        }
        if id == target {
            return Err(ScriptError::Runtime(
                "cannot attach an entity to itself".to_owned(),
            ));
        }
        let mut cursor = Some(target);
        while let Some(ancestor) = cursor {
            if ancestor == id {
                return Err(ScriptError::Runtime("attachment cycle detected".to_owned()));
            }
            cursor = self
                .by_id
                .get(&ancestor)
                .and_then(|slot| slot.state.attachment)
                .map(|att| att.parent);
        }

        let old_attachment = self.by_id.get(&id).and_then(|slot| slot.state.attachment);
        if let Some(old) = old_attachment
            && old.parent != target
        {
            self.detach_child_index(old.parent, id);
        }
        let list = self.children.entry(target).or_default();
        if !list.contains(&id) {
            list.push(id);
        }
        if old_attachment != Some(attachment) {
            if let Some(slot) = self.by_id.get_mut(&id) {
                slot.state.attachment = Some(attachment);
            }
            self.mark_state_dirty(id, STATE_ATTACHMENT);
        }
        self.apply_attached_transform(id, target, attachment);
        self.propagate_attachment_transforms(id);
        Ok(())
    }

    /// Detaches `id` from its parent if attached.
    pub(crate) fn detach(&mut self, id: u32) -> Result<(), ScriptError> {
        let slot = self
            .by_id
            .get_mut(&id)
            .ok_or_else(|| ScriptError::Runtime(format!("entity {id} does not exist")))?;
        if let Some(old) = slot.state.attachment.take() {
            self.detach_child_index(old.parent, id);
            self.mark_state_dirty(id, STATE_ATTACHMENT);
        }
        Ok(())
    }

    /// Updates `child_id`'s world transform from `parent_id`'s current world
    /// transform and `attachment`, marking it dirty and advancing its epoch.
    fn apply_attached_transform(
        &mut self,
        child_id: u32,
        parent_id: u32,
        attachment: EntityAttachment,
    ) {
        let Some((parent_pos, parent_rot)) = self
            .by_id
            .get(&parent_id)
            .map(|slot| (slot.info.position, slot.info.rotation))
        else {
            return;
        };
        let (world_pos, world_rot) = attached_world_transform(
            parent_pos,
            parent_rot,
            attachment.offset,
            attachment.rotation,
        );
        let Some(child_slot) = self.by_id.get_mut(&child_id) else {
            return;
        };
        let queued = child_slot.is_dirty();
        child_slot.info.position = world_pos;
        child_slot.info.rotation = world_rot;
        child_slot.position_dirty = true;
        child_slot.rotation_dirty = true;
        child_slot.epoch = child_slot.epoch.wrapping_add(1);
        if let Some(grid) = self.grid.as_mut() {
            grid.move_to(child_id, world_pos);
        }
        if !queued {
            self.dirty.push(child_id);
        }
    }

    /// Recomputes world transforms for all descendants attached to `root`.
    /// Returns immediately without allocating when `root` has no children.
    fn propagate_attachment_transforms(&mut self, root: u32) {
        if !self.children.contains_key(&root) {
            return;
        }
        let mut stack = vec![root];
        while let Some(parent_id) = stack.pop() {
            let Some(child_ids) = self.children.get(&parent_id).cloned() else {
                continue;
            };
            for child_id in child_ids {
                let Some(att) = self
                    .by_id
                    .get(&child_id)
                    .and_then(|slot| slot.state.attachment)
                else {
                    continue;
                };
                self.apply_attached_transform(child_id, parent_id, att);
                stack.push(child_id);
            }
        }
    }
}

/// Computes an attached child's world position and rotation from its parent's
/// world transform and the child's local offset and rotation.
fn attached_world_transform(
    parent_pos: [f32; 3],
    parent_rot: [f32; 3],
    offset: [f32; 3],
    local_rot: [f32; 3],
) -> ([f32; 3], [f32; 3]) {
    let rotated = rotate_euler(offset, parent_rot);
    let world_pos = [
        parent_pos[0] + rotated[0],
        parent_pos[1] + rotated[1],
        parent_pos[2] + rotated[2],
    ];
    let world_rot = [
        parent_rot[0] + local_rot[0],
        parent_rot[1] + local_rot[1],
        parent_rot[2] + local_rot[2],
    ];
    (world_pos, world_rot)
}

/// Rotates `offset` by Euler angles `(pitch, roll, yaw)` in degrees around X, Y, Z.
fn rotate_euler(offset: [f32; 3], rot_deg: [f32; 3]) -> [f32; 3] {
    if rot_deg == [0.0, 0.0, 0.0] {
        return offset;
    }
    let (sp, cp) = rot_deg[0].to_radians().sin_cos();
    let y1 = offset[1] * cp - offset[2] * sp;
    let z1 = offset[1] * sp + offset[2] * cp;
    let x1 = offset[0];

    let (sr, cr) = rot_deg[1].to_radians().sin_cos();
    let x2 = x1 * cr + z1 * sr;
    let z2 = -x1 * sr + z1 * cr;
    let y2 = y1;

    let (sy, cy) = rot_deg[2].to_radians().sin_cos();
    let x3 = x2 * cy - y2 * sy;
    let y3 = x2 * sy + y2 * cy;
    [x3, y3, z2]
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
        self.announce_created(id, info);
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
        let bucket = self.entities.borrow().bucket(id);
        let removed = self.entities.borrow_mut().remove(id);
        if removed.is_none() {
            return false;
        }
        self.state.borrow_mut().remove_bag(&entity_bag(id));
        self.announce_removed(id, bucket);
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
    pub(crate) fn player_transforms(&self, player: u32, updates: &[EntityUpdate]) {
        let bucket = self.bucket_of_player(player);
        let ignored = {
            let mut entities = self.entities.borrow_mut();
            updates
                .iter()
                .filter(|&&(id, position, rotation, epoch)| {
                    !entities.in_bucket(id, bucket)
                        || !entities.owner_move(player, id, position, rotation, epoch)
                })
                .count()
        };
        if ignored > 0 {
            debug!(player, ignored, "ignored entity transforms it may not send");
        }
    }

    /// Runs once per frame. Reads `sixm_cullRadius`, then sends the transforms
    /// and state updates queued since the last frame. With scoping on, each
    /// player's scope is recomputed first, the entities that entered or left it
    /// are sent, and each player gets only the updates of the entities it
    /// holds. With scoping off, the updates are broadcast (or routed per
    /// bucket). Nothing is sent when no entity changed. The update buffers are
    /// reused, so a frame does not allocate once warm.
    pub(crate) fn flush_entities(&self) {
        let radius = self.convars.borrow().cull_radius();
        self.apply_cull_radius(radius);
        let mut updates = std::mem::take(&mut *self.entity_updates.borrow_mut());
        let mut state_updates = std::mem::take(&mut *self.entity_state_updates.borrow_mut());
        {
            let mut entities = self.entities.borrow_mut();
            entities.take_dirty(&mut updates);
            entities.take_state_dirty(&mut state_updates);
        }
        if self.scoping() {
            self.refresh_scopes();
            self.send_scoped_transforms(&updates);
            self.send_scoped_states(&state_updates);
        } else {
            if !updates.is_empty() {
                if self.buckets_in_use() {
                    self.send_bucketed_transforms(&updates);
                } else {
                    self.with_server(|server| {
                        if let Err(err) = server.broadcast_entity_transforms(&updates) {
                            warn!("cannot send the entity transforms: {err}");
                        }
                    });
                }
            }
            if !state_updates.is_empty() {
                if self.buckets_in_use() {
                    self.send_bucketed_states(&state_updates);
                } else {
                    self.with_server(|server| {
                        if let Err(err) = server.broadcast_entity_states(&state_updates) {
                            warn!("cannot send the entity states: {err}");
                        }
                    });
                }
            }
        }
        updates.clear();
        state_updates.clear();
        *self.entity_updates.borrow_mut() = updates;
        *self.entity_state_updates.borrow_mut() = state_updates;
    }

    /// Every entity, in ascending id order, for the console.
    pub(crate) fn entity_list(&self) -> Vec<(u32, EntityInfo, EntityState, u32)> {
        self.entities.borrow().all()
    }

    /// Sends `player`, who is being admitted, the entities it holds: every
    /// entity, or its scope while scoping is on.
    pub(crate) fn join_entities(&self, player: u32) {
        let entries = self.join_scope(player);
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
            vec![(id, Some([2.0, 0.0, 0.0]), Some([0.0, 0.0, 90.0]), 3)],
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
            vec![(kept, Some([6.0, 0.0, 0.0]), None, 1)]
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
        assert!(store.owner_move(4, ped, Some(OWNER_POSITION), Some([0.0; 3]), 0));
        assert_eq!(
            dirty(&mut store),
            vec![(ped, Some(OWNER_POSITION), Some([0.0; 3]), 0)],
            "an owner move keeps epoch 0"
        );

        store.set_position(ped, [1.0, 1.0, 1.0]).unwrap();
        assert_eq!(
            dirty(&mut store),
            vec![(ped, Some([1.0, 1.0, 1.0]), None, 1)],
            "a server move starts epoch 1"
        );
        assert!(store.owner_move(4, ped, Some(OWNER_POSITION), Some([0.0; 3]), 1));
        assert_eq!(
            dirty(&mut store),
            vec![(ped, Some(OWNER_POSITION), Some([0.0; 3]), 1)],
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
        assert!(!store.owner_move(4, ped, Some(OWNER_POSITION), Some([0.0; 3]), 0));
        assert_eq!(
            dirty(&mut store),
            vec![(ped, Some([100.0, 0.0, 0.0]), None, 1)]
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
        assert!(!store.owner_move(4, ped, Some(OWNER_POSITION), Some([0.0; 3]), 0));
        assert_eq!(store.get(ped).unwrap().position, [100.0, 0.0, 0.0]);
        assert!(store.owner_move(4, ped, Some(OWNER_POSITION), Some([0.0; 3]), 1));
        assert_eq!(store.get(ped).unwrap().position, OWNER_POSITION);
        assert_eq!(
            dirty(&mut store),
            vec![(ped, Some(OWNER_POSITION), Some([0.0; 3]), 1)]
        );
    }

    #[test]
    fn the_epoch_wraps_around() {
        let mut store = EntityStore::default();
        let ped = store.insert(owned_by(4)).unwrap();
        store.by_id.get_mut(&ped).unwrap().epoch = u32::MAX;
        store.set_position(ped, [1.0, 0.0, 0.0]).unwrap();
        assert_eq!(
            dirty(&mut store),
            vec![(ped, Some([1.0, 0.0, 0.0]), None, 0)]
        );
    }

    #[test]
    fn owner_moves_are_refused_for_the_wrong_player_or_entity() {
        let mut store = EntityStore::default();
        let ped = store.insert(owned_by(4)).unwrap();
        let vehicle = store.insert(info(EntityKind::Vehicle, 0.0)).unwrap();

        assert!(
            !store.owner_move(5, ped, Some(OWNER_POSITION), Some([0.0; 3]), 0),
            "not the owner"
        );
        assert!(
            !store.owner_move(4, vehicle, Some(OWNER_POSITION), Some([0.0; 3]), 0),
            "script-created"
        );
        assert!(
            !store.owner_move(4, 99, Some(OWNER_POSITION), Some([0.0; 3]), 0),
            "unknown entity"
        );
        assert!(dirty(&mut store).is_empty(), "nothing is queued");
        assert_eq!(store.get(ped).unwrap().position, [0.0; 3]);
    }

    #[test]
    fn owner_moves_with_a_value_that_is_not_finite_are_refused() {
        let mut store = EntityStore::default();
        let ped = store.insert(owned_by(4)).unwrap();
        assert!(!store.owner_move(4, ped, Some([f32::NAN, 0.0, 0.0]), Some([0.0; 3]), 0));
        assert!(!store.owner_move(4, ped, Some([0.0; 3]), Some([0.0, f32::INFINITY, 0.0]), 0));
        assert!(dirty(&mut store).is_empty(), "nothing is queued");
    }

    #[test]
    fn the_owned_index_follows_inserts_and_removals() {
        let mut store = EntityStore::default();
        let first = store.insert(owned_by(4)).unwrap();
        let second = store.insert(owned_by(4)).unwrap();
        let other = store.insert(owned_by(5)).unwrap();
        assert_eq!(store.owned(4), &[first, second]);
        assert_eq!(store.owned(5), &[other]);
        assert_eq!(store.owned(6), &[] as &[u32]);

        store.remove(first);
        assert_eq!(store.owned(4), &[second]);
        store.remove(second);
        assert_eq!(store.owned(4), &[] as &[u32]);
        assert!(!store.owned.contains_key(&4), "an empty list is dropped");
    }

    #[test]
    fn a_new_grid_indexes_the_entities_already_there() {
        let mut store = EntityStore::default();
        let near = store.insert(info(EntityKind::Ped, 80.0)).unwrap();
        let mut scope = Vec::new();
        store.set_grid(Some(10.0));
        store.scope_into(1, Some([0.0; 3]), 100.0, 0, &mut scope);
        assert!(
            scope.is_empty(),
            "a grid of 10 metres does not reach 80 metres"
        );

        store.set_grid(Some(100.0));
        store.scope_into(1, Some([0.0; 3]), 100.0, 0, &mut scope);
        assert_eq!(scope, vec![near]);

        store.set_grid(None);
        store.scope_into(1, Some([0.0; 3]), 100.0, 0, &mut scope);
        assert!(scope.is_empty(), "without a grid nothing is near");
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

    #[test]
    fn a_heading_only_change_sends_only_the_rotation() {
        let mut store = EntityStore::default();
        let vehicle = store.insert(info(EntityKind::Vehicle, 4.0)).unwrap();
        store.set_rotation(vehicle, [0.0, 0.0, 45.0]).unwrap();
        assert_eq!(
            dirty(&mut store),
            vec![(vehicle, None, Some([0.0, 0.0, 45.0]), 1)],
            "the position did not change, so it is not sent"
        );
    }

    #[test]
    fn a_position_and_a_rotation_in_one_frame_send_both_parts() {
        let mut store = EntityStore::default();
        let vehicle = store.insert(info(EntityKind::Vehicle, 0.0)).unwrap();
        store.set_rotation(vehicle, [0.0, 0.0, 45.0]).unwrap();
        store.set_position(vehicle, [1.0, 0.0, 0.0]).unwrap();
        assert_eq!(
            dirty(&mut store),
            vec![(vehicle, Some([1.0, 0.0, 0.0]), Some([0.0, 0.0, 45.0]), 2)]
        );
    }

    #[test]
    fn an_owner_sends_only_the_part_it_moved_and_keeps_the_other() {
        let mut store = EntityStore::default();
        let ped = store.insert(owned_by(4)).unwrap();
        assert!(store.owner_move(4, ped, None, Some([0.0, 0.0, 90.0]), 0));
        assert_eq!(
            dirty(&mut store),
            vec![(ped, None, Some([0.0, 0.0, 90.0]), 0)]
        );
        assert_eq!(store.get(ped).unwrap().position, [0.0; 3]);
        assert_eq!(store.get(ped).unwrap().rotation, [0.0, 0.0, 90.0]);
    }

    #[test]
    fn an_owner_update_with_no_part_is_refused() {
        let mut store = EntityStore::default();
        let ped = store.insert(owned_by(4)).unwrap();
        assert!(!store.owner_move(4, ped, None, None, 0));
        assert!(dirty(&mut store).is_empty(), "nothing is queued");
    }

    #[test]
    fn a_part_already_queued_is_not_queued_twice() {
        let mut store = EntityStore::default();
        let ped = store.insert(owned_by(4)).unwrap();
        assert!(store.owner_move(4, ped, Some([1.0, 0.0, 0.0]), None, 0));
        assert!(store.owner_move(4, ped, None, Some([0.0, 0.0, 10.0]), 0));
        assert_eq!(
            dirty(&mut store),
            vec![(ped, Some([1.0, 0.0, 0.0]), Some([0.0, 0.0, 10.0]), 0)]
        );
    }

    #[test]
    fn a_scope_holds_only_the_entities_of_its_bucket() {
        let mut store = EntityStore::default();
        store.set_grid(Some(50.0));
        let near = store.insert(info(EntityKind::Ped, 1.0)).unwrap();
        let far_owned = store.insert(owned_by(4)).unwrap();
        let moved = store.insert(owned_by(4)).unwrap();
        store.set_bucket(near, 7);
        store.set_bucket(moved, 7);
        let mut scope = Vec::new();
        store.scope_into(4, Some([0.0; 3]), 50.0, 0, &mut scope);
        assert_eq!(
            scope,
            vec![far_owned],
            "the owned entity in bucket 7 is not held"
        );
        store.scope_into(4, Some([0.0; 3]), 50.0, 7, &mut scope);
        assert_eq!(scope, vec![near, moved], "bucket 7 holds its own entities");
        store.scope_into(4, Some([0.0; 3]), 50.0, 3, &mut scope);
        assert!(scope.is_empty(), "an empty bucket holds nothing");
    }

    #[test]
    fn buckets_are_kept_sparse_and_follow_removals() {
        let mut store = EntityStore::default();
        let id = store.insert(info(EntityKind::Object, 0.0)).unwrap();
        assert!(!store.any_bucket());
        assert!(store.in_bucket(id, 0) && !store.in_bucket(id, 1));
        store.set_bucket(id, 5);
        assert!(store.any_bucket() && store.bucket(id) == 5);
        store.set_bucket(id, 0);
        assert!(!store.any_bucket(), "bucket 0 is not stored");
        store.set_bucket(id, 9);
        store.remove(id);
        assert!(!store.any_bucket(), "a removed entity drops its bucket");
        store.set_bucket(id, 9);
        assert!(!store.any_bucket(), "an unknown entity cannot be moved");
    }
}

#[cfg(test)]
mod client_store_tests {
    use super::*;

    fn at(kind: EntityKind) -> EntityInfo {
        EntityInfo {
            kind,
            model: 1,
            position: [1.0, 2.0, 3.0],
            rotation: [0.0; 3],
            owner: None,
        }
    }

    #[test]
    fn leases_hand_out_blocks_that_no_script_entity_takes() {
        let mut store = EntityStore::default();
        assert_eq!(store.lease_ids(4, 3), Some((1, 3)));
        assert_eq!(store.leased_remaining(4), 3);
        let script = store.insert(at(EntityKind::Object)).expect("script entity");
        assert_eq!(script, 4, "the leased block is skipped");
        assert_eq!(store.lease_ids(5, 2), Some((5, 2)));
        assert_eq!(store.leased_remaining(5), 2);
        assert_eq!(store.leased_remaining(9), 0, "an unknown player has none");
    }

    #[test]
    fn leased_ids_are_used_in_order_and_only_once() {
        let mut store = EntityStore::default();
        store.lease_ids(4, 2).expect("block");
        assert!(!store.use_leased_id(4, 2), "the first id is 1, not 2");
        assert!(store.use_leased_id(4, 1));
        assert!(!store.use_leased_id(4, 1), "an id is used once");
        assert!(store.use_leased_id(4, 2));
        assert_eq!(store.leased_remaining(4), 0);
        assert!(!store.use_leased_id(4, 3), "the block is used up");
        assert!(!store.use_leased_id(7, 1), "no block for another player");
    }

    #[test]
    fn leased_ids_run_out_before_the_last_id() {
        let mut store = EntityStore {
            next_id: u32::MAX - 1,
            ..EntityStore::default()
        };
        assert_eq!(store.lease_ids(4, 1), Some((u32::MAX - 1, 1)));
        assert_eq!(store.lease_ids(4, 5), None, "the ids would overflow");
    }

    #[test]
    fn client_entities_count_against_their_creator() {
        let mut store = EntityStore::default();
        store.lease_ids(4, 2).expect("block");
        assert!(store.use_leased_id(4, 1));
        let owned_by_creator = EntityInfo {
            owner: Some(4),
            ..at(EntityKind::Vehicle)
        };
        store
            .insert_created(1, owned_by_creator, 4)
            .expect("created");
        assert_eq!(store.created_by(4), &[1]);
        assert_eq!(store.owned(4), &[1], "the creator owns its entity");
        assert!(store.insert_created(1, owned_by_creator, 4).is_err());

        store.remove(1);
        assert_eq!(store.created_by(4), &[] as &[u32]);
    }

    #[test]
    fn a_created_entity_must_be_finite_and_within_the_limit() {
        let mut store = EntityStore::default();
        let mut bad = at(EntityKind::Ped);
        bad.rotation = [f32::NAN, 0.0, 0.0];
        assert!(store.insert_created(1, bad, 4).is_err());
        assert!(store.get(1).is_none());
    }

    #[test]
    fn a_handover_keeps_the_epoch_and_moves_the_owner_index() {
        let mut store = EntityStore::default();
        let ped = store.insert(at(EntityKind::Ped)).expect("entity");
        store
            .set_position(ped, [9.0, 9.0, 9.0])
            .expect("server move");
        assert_eq!(store.info_and_epoch(ped).map(|(_, epoch)| epoch), Some(1));

        assert!(store.set_owner(ped, Some(4)));
        assert_eq!(store.owned(4), &[ped]);
        assert_eq!(store.info_and_epoch(ped).map(|(_, epoch)| epoch), Some(1));
        assert!(
            store.owner_move(4, ped, Some([1.0; 3]), None, 1),
            "the new owner starts at the current epoch"
        );

        assert!(
            !store.set_owner(ped, Some(4)),
            "the same owner changes nothing"
        );
        assert!(store.set_owner(ped, Some(5)));
        assert_eq!(store.owned(4), &[] as &[u32]);
        assert_eq!(store.owned(5), &[ped]);
        assert!(store.set_owner(ped, None));
        assert_eq!(store.owned(5), &[] as &[u32]);
        assert!(!store.set_owner(99, Some(4)), "an unknown entity");
    }

    #[test]
    fn an_orphaned_creation_can_be_kept_by_releasing_its_creator() {
        let mut store = EntityStore::default();
        store.lease_ids(4, 2).expect("block");
        assert!(store.use_leased_id(4, 1));
        store
            .insert_created(1, at(EntityKind::Object), 4)
            .expect("created");
        assert!(!store.migrates_on_orphan(1));
        store.set_migrates_on_orphan(1, true);
        assert!(store.migrates_on_orphan(1));

        store.release_creator(1);
        assert_eq!(store.created_by(4), &[] as &[u32]);
        assert!(store.get(1).is_some(), "the entity stays");
        store.drop_leases(4);
        assert_eq!(store.leased_remaining(4), 0);
    }

    #[test]
    fn freezing_blocks_owner_moves_but_allows_server_moves() {
        let mut store = EntityStore::default();
        let ped = store
            .insert(EntityInfo {
                owner: Some(4),
                ..at(EntityKind::Ped)
            })
            .unwrap();
        store.set_frozen(ped, true).unwrap();
        assert!(!store.owner_move(4, ped, Some([9.0, 9.0, 9.0]), None, 0));
        assert_eq!(store.get(ped).unwrap().position, [1.0, 2.0, 3.0]);

        // Server move still succeeds and advances epoch.
        store.set_position(ped, [5.0, 6.0, 7.0]).unwrap();
        assert_eq!(store.get(ped).unwrap().position, [5.0, 6.0, 7.0]);

        // Unfreezing allows owner moves at the new epoch.
        store.set_frozen(ped, false).unwrap();
        assert!(store.owner_move(4, ped, Some([8.0, 8.0, 8.0]), None, 1));
        assert_eq!(store.get(ped).unwrap().position, [8.0, 8.0, 8.0]);
    }

    #[test]
    fn state_deltas_coalesce_and_skip_unchanged_fields() {
        let mut store = EntityStore::default();
        let id = store.insert(at(EntityKind::Vehicle)).unwrap();
        let mut updates = Vec::new();
        store.take_state_dirty(&mut updates);
        assert_eq!(updates, Vec::new());

        // Setting defaults produces no dirty updates.
        store.set_frozen(id, false).unwrap();
        store.set_visible(id, true).unwrap();
        store.set_alpha(id, 255).unwrap();
        store.set_health(id, 200).unwrap();
        store.set_max_health(id, 200).unwrap();
        store.set_velocity(id, [0.0; 3]).unwrap();
        store.take_state_dirty(&mut updates);
        assert_eq!(updates, Vec::new());

        store.set_health(id, -10).unwrap();
        store.set_health(id, 125).unwrap();
        store.set_alpha(id, 100).unwrap();
        store.set_visible(id, false).unwrap();
        store.set_velocity(id, [1.0, 2.0, 3.0]).unwrap();
        store.set_max_health(id, 500).unwrap();
        store.take_state_dirty(&mut updates);
        assert_eq!(
            updates,
            vec![EntityStateUpdate {
                id,
                frozen: None,
                velocity: Some([1.0, 2.0, 3.0]),
                health: Some(125),
                max_health: Some(500),
                visible: Some(false),
                alpha: Some(100),
                attachment: None,
            }]
        );
        updates.clear();
        store.take_state_dirty(&mut updates);
        assert_eq!(updates, Vec::new());
        assert!(store.set_velocity(id, [f32::NAN, 0.0, 0.0]).is_err());
    }

    #[test]
    fn attachment_prevents_self_and_cycles() {
        let mut store = EntityStore::default();
        let parent = store.insert(at(EntityKind::Vehicle)).unwrap();
        let child = store.insert(at(EntityKind::Object)).unwrap();
        let grand = store.insert(at(EntityKind::Object)).unwrap();

        let att = |to| EntityAttachment {
            parent: to,
            bone_index: 0,
            offset: [0.0; 3],
            rotation: [0.0; 3],
        };
        assert!(store.attach(parent, att(parent)).is_err());
        store.attach(child, att(parent)).unwrap();
        store.attach(grand, att(child)).unwrap();
        assert!(store.attach(parent, att(grand)).is_err());
    }

    #[test]
    fn attachment_follows_parent_blocks_owner_moves_and_detaches_on_delete() {
        let mut store = EntityStore::default();
        let parent = store
            .insert(EntityInfo {
                owner: Some(1),
                ..at(EntityKind::Vehicle)
            })
            .unwrap();
        let child = store
            .insert(EntityInfo {
                owner: Some(2),
                ..at(EntityKind::Object)
            })
            .unwrap();
        let grand = store.insert(at(EntityKind::Object)).unwrap();

        store
            .attach(
                child,
                EntityAttachment {
                    parent,
                    bone_index: 3,
                    offset: [1.0, 0.0, 0.0],
                    rotation: [0.0, 0.0, 10.0],
                },
            )
            .unwrap();
        store
            .attach(
                grand,
                EntityAttachment {
                    parent: child,
                    bone_index: 0,
                    offset: [0.0, 2.0, 0.0],
                    rotation: [0.0; 3],
                },
            )
            .unwrap();

        let child_epoch = store.info_and_epoch(child).unwrap().1;
        assert!(!store.owner_move(2, child, Some([50.0, 50.0, 50.0]), None, child_epoch));

        let mut dirty_transforms = Vec::new();
        store.take_dirty(&mut dirty_transforms);
        dirty_transforms.clear();

        assert!(store.owner_move(
            1,
            parent,
            Some([10.0, 20.0, 30.0]),
            Some([0.0, 0.0, 90.0]),
            0
        ));
        let child_pos = store.get(child).unwrap().position;
        assert!((child_pos[0] - 10.0).abs() < 1e-4);
        assert!((child_pos[1] - 21.0).abs() < 1e-4);
        assert!((child_pos[2] - 30.0).abs() < 1e-4);
        assert_eq!(store.get(child).unwrap().rotation, [0.0, 0.0, 100.0]);

        store.take_dirty(&mut dirty_transforms);
        assert!(dirty_transforms.iter().any(|u| u.0 == child));
        assert!(dirty_transforms.iter().any(|u| u.0 == grand));

        let mut state_updates = Vec::new();
        store.take_state_dirty(&mut state_updates);
        state_updates.clear();

        store.remove(parent);
        assert_eq!(store.state(child).unwrap().attachment, None);
        store.take_state_dirty(&mut state_updates);
        assert_eq!(
            state_updates,
            vec![EntityStateUpdate {
                id: child,
                frozen: None,
                velocity: None,
                health: None,
                max_health: None,
                visible: None,
                alpha: None,
                attachment: Some(None),
            }]
        );
    }
}
