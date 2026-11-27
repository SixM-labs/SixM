//! Entity scoping: which entities each player is sent.
//!
//! With `sixm_cullRadius` above zero, the scope of a player holds every entity
//! within the radius of its ped and every entity the player owns. A player
//! knows the entities of its scope and no others. An entity entering the scope
//! is created for the player, and one leaving it is deleted for the player, so
//! its scripts see the entity disappear.
//!
//! The scope of each player is recomputed once per frame. Entities are found
//! through a uniform grid over x and y whose cells are as wide as the radius,
//! so a scope is computed from the nine cells around the ped. The new scope is
//! then compared with the old one by a linear merge, and only the differences
//! are sent.

use std::cmp::Ordering;
use std::collections::{BTreeMap, HashMap};

use sixm_core::{EntityInfo, ScriptError, Value, entity_bag};
use sixm_net::{EntityEntry, EntityStateUpdate, NetServer};
use tracing::warn;

use crate::entities::{EntityStore, EntityUpdate};
use crate::manager::ResourceManager;

/// A cell of the grid: the position divided by the cell size, rounded down,
/// for x and y.
type Cell = (i64, i64);

/// Where an entity sits in the grid: its cell, and its index in the cell's list.
#[derive(Debug, Clone, Copy)]
struct Location {
    cell: Cell,
    index: usize,
}

/// Entities bucketed by the cell of their x and y, for finding the entities
/// near a point.
///
/// Cells are as wide as the radius, so every entity within the radius of a
/// point is in one of the nine cells around the point's cell. Each entity is in
/// exactly one cell, and `locations` finds it in constant time. A removal moves
/// the last entity of its cell into the gap.
#[derive(Debug)]
pub(crate) struct Grid {
    size: f64,
    cells: HashMap<Cell, Vec<(u32, [f32; 3])>>,
    locations: HashMap<u32, Location>,
}

impl Grid {
    /// An empty grid whose cells are `size` metres wide.
    pub(crate) fn new(size: f32) -> Self {
        Self {
            size: f64::from(size),
            cells: HashMap::new(),
            locations: HashMap::new(),
        }
    }

    /// The cell of `position`. Casts from float saturate, so positions beyond
    /// the range of `i64` share the outermost cell. Neighbouring cells stay
    /// neighbours, and the distance test is still exact.
    #[expect(
        clippy::cast_possible_truncation,
        reason = "the saturating cast is the intended behaviour"
    )]
    fn cell_of(&self, position: [f32; 3]) -> Cell {
        let index = |value: f32| (f64::from(value) / self.size).floor() as i64;
        (index(position[0]), index(position[1]))
    }

    /// Adds entity `id` at `position`.
    pub(crate) fn insert(&mut self, id: u32, position: [f32; 3]) {
        let cell = self.cell_of(position);
        self.place(id, cell, position);
    }

    /// Removes entity `id`, when it is in the grid.
    pub(crate) fn remove(&mut self, id: u32) {
        if let Some(location) = self.locations.remove(&id) {
            self.detach(location);
        }
    }

    /// Records that entity `id` is now at `position`. It changes cell only when
    /// it crosses a cell border, and then costs constant time.
    pub(crate) fn move_to(&mut self, id: u32, position: [f32; 3]) {
        let cell = self.cell_of(position);
        let Some(location) = self.locations.get(&id).copied() else {
            return;
        };
        if location.cell != cell {
            self.detach(location);
            self.place(id, cell, position);
        } else if let Some(bucket) = self.cells.get_mut(&cell) {
            bucket[location.index].1 = position;
        }
    }

    /// Appends entity `id` to `cell`.
    fn place(&mut self, id: u32, cell: Cell, position: [f32; 3]) {
        let bucket = self.cells.entry(cell).or_default();
        bucket.push((id, position));
        let index = bucket.len() - 1;
        self.locations.insert(id, Location { cell, index });
    }

    /// Takes the entity at `location` out of its cell. The last entity of the
    /// cell takes its place, and the cell is dropped once it is empty. The
    /// entity's own entry in `locations` is left to the caller.
    fn detach(&mut self, location: Location) {
        let Some(bucket) = self.cells.get_mut(&location.cell) else {
            return;
        };
        bucket.swap_remove(location.index);
        if let Some((moved, _)) = bucket.get(location.index).copied()
            && let Some(found) = self.locations.get_mut(&moved)
        {
            found.index = location.index;
        }
        if bucket.is_empty() {
            self.cells.remove(&location.cell);
        }
    }

    /// Calls `visit` with every entity in the nine cells around `position`.
    /// Entities farther away may be included, so the caller measures them.
    pub(crate) fn for_each_near(&self, position: [f32; 3], mut visit: impl FnMut(u32, [f32; 3])) {
        let (x, y) = self.cell_of(position);
        for cx in x.saturating_sub(1)..=x.saturating_add(1) {
            for cy in y.saturating_sub(1)..=y.saturating_add(1) {
                if let Some(bucket) = self.cells.get(&(cx, cy)) {
                    for &(id, position) in bucket {
                        visit(id, position);
                    }
                }
            }
        }
    }
}

/// The squared distance between two points.
pub(crate) fn squared_distance(a: [f32; 3], b: [f32; 3]) -> f32 {
    let dx = a[0] - b[0];
    let dy = a[1] - b[1];
    let dz = a[2] - b[2];
    dx * dx + dy * dy + dz * dz
}

/// Splits the change from `old` to `new`, both sorted ascending, into the ids
/// only in `new` (`entered`) and the ids only in `old` (`left`). The two lists
/// are cleared first.
fn diff(old: &[u32], new: &[u32], entered: &mut Vec<u32>, left: &mut Vec<u32>) {
    entered.clear();
    left.clear();
    let (mut i, mut j) = (0, 0);
    while i < old.len() && j < new.len() {
        match old[i].cmp(&new[j]) {
            Ordering::Less => {
                left.push(old[i]);
                i += 1;
            }
            Ordering::Greater => {
                entered.push(new[j]);
                j += 1;
            }
            Ordering::Equal => {
                i += 1;
                j += 1;
            }
        }
    }
    left.extend_from_slice(&old[i..]);
    entered.extend_from_slice(&new[j..]);
}

/// The bucket of `player` in `buckets`, where a player without an entry is in
/// bucket 0.
fn bucket_in(buckets: &BTreeMap<u32, u32>, player: u32) -> u32 {
    buckets.get(&player).copied().unwrap_or(0)
}

/// The scope state of the server.
///
/// Each admitted player has a sorted list of the entity ids it was sent and
/// holds. A sorted list is used because the scope is rebuilt every frame and
/// compared with a linear merge, and because membership is a binary search.
/// Every list is kept while scoping is off, but stays empty then.
#[derive(Debug, Default)]
pub(crate) struct Scopes {
    /// The radius in metres in use. Zero while scoping is off.
    radius: f32,
    /// Every admitted player, with the ids of the entities it holds.
    pub(crate) players: BTreeMap<u32, Vec<u32>>,
    /// The routing bucket of each player outside bucket 0. A player without
    /// an entry is in bucket 0.
    pub(crate) buckets: BTreeMap<u32, u32>,
    /// Scratch lists reused between frames: the scope being computed, the
    /// entities that entered and left, and the per-player update batches.
    next: Vec<u32>,
    entered: Vec<u32>,
    left: Vec<u32>,
    batch: Vec<EntityUpdate>,
    state_batch: Vec<EntityStateUpdate>,
}

impl ResourceManager {
    /// Whether scoping is on, as applied at the last frame.
    pub(crate) fn scoping(&self) -> bool {
        self.scopes.borrow().radius > 0.0
    }

    /// The routing bucket of `player`. Zero for a player that is not connected.
    pub(crate) fn bucket_of_player(&self, player: u32) -> u32 {
        bucket_in(&self.scopes.borrow().buckets, player)
    }

    /// Whether any player or entity is outside bucket 0. While none is, the
    /// broadcast paths are used unchanged.
    pub(crate) fn buckets_in_use(&self) -> bool {
        self.entities.borrow().any_bucket() || !self.scopes.borrow().buckets.is_empty()
    }

    /// Whether `player` holds `entity`, or may hold it. With scoping off every
    /// client holds every entity of its bucket, so this is the bucket test.
    pub(crate) fn in_scope(&self, player: u32, entity: u32) -> bool {
        let scopes = self.scopes.borrow();
        if scopes.radius <= 0.0 {
            return self
                .entities
                .borrow()
                .in_bucket(entity, bucket_in(&scopes.buckets, player));
        }
        scopes
            .players
            .get(&player)
            .is_some_and(|scope| scope.binary_search(&entity).is_ok())
    }

    /// Adds `player` to the scopes and returns the entities it is sent when it
    /// joins: its scope, or with scoping off every entity of its bucket.
    pub(crate) fn join_scope(&self, player: u32) -> Vec<EntityEntry> {
        let mut guard = self.scopes.borrow_mut();
        let scopes = &mut *guard;
        let bucket = bucket_in(&scopes.buckets, player);
        if scopes.radius <= 0.0 {
            scopes.players.insert(player, Vec::new());
            let entities = self.entities.borrow();
            return entities
                .all()
                .into_iter()
                .filter(|(id, _, _, _)| entities.in_bucket(*id, bucket))
                .map(EntityEntry::from)
                .collect();
        }
        let entities = self.entities.borrow();
        let center = self
            .ped_of_player(player)
            .and_then(|ped| entities.position(ped));
        entities.scope_into(player, center, scopes.radius, bucket, &mut scopes.next);
        let entries = scopes
            .next
            .iter()
            .filter_map(|&id| {
                entities
                    .info_state_and_epoch(id)
                    .map(|(info, state, epoch)| EntityEntry::from((id, info, state, epoch)))
            })
            .collect();
        scopes.players.insert(player, scopes.next.clone());
        entries
    }

    /// Removes `player` from the scopes and the buckets, once it has left.
    pub(crate) fn leave_scope(&self, player: u32) {
        let mut scopes = self.scopes.borrow_mut();
        scopes.players.remove(&player);
        scopes.buckets.remove(&player);
    }

    /// Tells the players that can see the new entity `id` about it, and adds it
    /// to their scopes. Only players in the entity's bucket can see it. With
    /// scoping on, a player sees it when it is owned by the player or within
    /// the radius of the player's ped. With scoping off, every player of the
    /// bucket is told.
    pub(crate) fn announce_created(&self, id: u32, info: EntityInfo) {
        let mut guard = self.scopes.borrow_mut();
        let scopes = &mut *guard;
        let entities = self.entities.borrow();
        let state = entities.state(id).unwrap_or_default();
        let entry = EntityEntry::from((id, info, state, 0));
        let bucket = entities.bucket(id);
        if scopes.radius <= 0.0 {
            if bucket == 0 && scopes.buckets.is_empty() {
                self.with_server(|server| {
                    if let Err(err) = server.broadcast_entity_created(entry) {
                        warn!("cannot announce entity {id}: {err}");
                    }
                });
                return;
            }
            for &player in scopes.players.keys() {
                if bucket_in(&scopes.buckets, player) != bucket {
                    continue;
                }
                self.with_server(|server| {
                    if let Err(err) = server.send_entity_created(player, entry) {
                        warn!("cannot send entity {id} to player {player}: {err}");
                    }
                });
            }
            return;
        }
        let limit = scopes.radius * scopes.radius;
        for (&player, scope) in &mut scopes.players {
            if bucket_in(&scopes.buckets, player) != bucket {
                continue;
            }
            let center = self
                .ped_of_player(player)
                .and_then(|ped| entities.position(ped));
            let visible = info.owner == Some(player)
                || center.is_some_and(|at| squared_distance(at, info.position) <= limit);
            if !visible {
                continue;
            }
            if let Err(at) = scope.binary_search(&id) {
                scope.insert(at, id);
            }
            self.with_server(|server| {
                if let Err(err) = server.send_entity_created(player, entry) {
                    warn!("cannot send entity {id} to player {player}: {err}");
                }
            });
        }
    }

    /// Tells the players that hold the entity `id`, which was in `bucket`, that
    /// it was deleted, and forgets it in their scopes. Its state bag is already
    /// removed. With scoping off, every player of the bucket is told.
    pub(crate) fn announce_removed(&self, id: u32, bucket: u32) {
        let mut guard = self.scopes.borrow_mut();
        let scopes = &mut *guard;
        if scopes.radius <= 0.0 {
            if bucket == 0 && scopes.buckets.is_empty() {
                let bag = entity_bag(id);
                self.with_server(|server| {
                    if let Err(err) = server.broadcast_state_removed(&bag) {
                        warn!("cannot announce the removal of state bag `{bag}`: {err}");
                    }
                    if let Err(err) = server.broadcast_entity_deleted(id) {
                        warn!("cannot announce the deletion of entity {id}: {err}");
                    }
                });
                return;
            }
            for &player in scopes.players.keys() {
                if bucket_in(&scopes.buckets, player) == bucket {
                    self.send_removed_to(player, id);
                }
            }
            return;
        }
        for (&player, scope) in &mut scopes.players {
            if let Ok(at) = scope.binary_search(&id) {
                scope.remove(at);
                self.send_removed_to(player, id);
            }
        }
    }

    /// Sends a replicated write to an entity bag to the players that hold the
    /// entity. Every other bag is broadcast.
    pub(crate) fn send_entity_state(
        &self,
        server: &NetServer,
        entity: u32,
        bag: &str,
        key: &str,
        value: &Value,
    ) -> Result<(), ScriptError> {
        let scopes = self.scopes.borrow();
        for (&player, scope) in &scopes.players {
            if scope.binary_search(&entity).is_ok() {
                server
                    .send_state(player, bag, key, value)
                    .map_err(|err| ScriptError::Runtime(err.to_string()))?;
            }
        }
        Ok(())
    }

    /// Sends a replicated write to an entity bag to the players of `bucket`,
    /// for scoping off while buckets are in use.
    pub(crate) fn send_state_in_bucket(
        &self,
        server: &NetServer,
        bucket: u32,
        bag: &str,
        key: &str,
        value: &Value,
    ) -> Result<(), ScriptError> {
        let scopes = self.scopes.borrow();
        for &player in scopes.players.keys() {
            if bucket_in(&scopes.buckets, player) == bucket {
                server
                    .send_state(player, bag, key, value)
                    .map_err(|err| ScriptError::Runtime(err.to_string()))?;
            }
        }
        Ok(())
    }

    /// Applies the radius read at the start of a frame.
    ///
    /// Turning scoping on or off changes what the clients hold, so they are
    /// brought up to date here. When it turns on, every client is taken to hold
    /// every entity of its bucket, and [`refresh_scopes`](Self::refresh_scopes)
    /// then shrinks the scopes. When it turns off, every client is sent the
    /// entities of its bucket it does not hold. A new radius rebuilds the grid.
    pub(crate) fn apply_cull_radius(&self, radius: f32) {
        let mut guard = self.scopes.borrow_mut();
        let scopes = &mut *guard;
        if scopes.radius.to_bits() == radius.to_bits() {
            return;
        }
        let mut entities = self.entities.borrow_mut();
        if radius > 0.0 {
            if scopes.radius <= 0.0 {
                let all: Vec<u32> = entities.ids_ascending().collect();
                for (&player, scope) in &mut scopes.players {
                    let bucket = bucket_in(&scopes.buckets, player);
                    scope.clear();
                    scope.extend(
                        all.iter()
                            .copied()
                            .filter(|&id| entities.in_bucket(id, bucket)),
                    );
                }
            }
            entities.set_grid(Some(radius));
        } else {
            for (&player, scope) in &mut scopes.players {
                let bucket = bucket_in(&scopes.buckets, player);
                let mut held = scope.iter().copied().peekable();
                for id in entities.ids_ascending() {
                    while held.peek().is_some_and(|&known| known < id) {
                        held.next();
                    }
                    if held.peek() != Some(&id) && entities.in_bucket(id, bucket) {
                        self.send_entity_to(player, id, &entities);
                    }
                }
                scope.clear();
            }
            entities.set_grid(None);
        }
        scopes.radius = radius;
    }

    /// Recomputes the scope of every player, and sends the entities that
    /// entered or left each one.
    pub(crate) fn refresh_scopes(&self) {
        let mut guard = self.scopes.borrow_mut();
        let scopes = &mut *guard;
        let entities = self.entities.borrow();
        for (&player, scope) in &mut scopes.players {
            let center = self
                .ped_of_player(player)
                .and_then(|ped| entities.position(ped));
            let bucket = bucket_in(&scopes.buckets, player);
            entities.scope_into(player, center, scopes.radius, bucket, &mut scopes.next);
            diff(scope, &scopes.next, &mut scopes.entered, &mut scopes.left);
            for &id in &scopes.entered {
                self.send_entity_to(player, id, &entities);
            }
            for &id in &scopes.left {
                self.send_removed_to(player, id);
            }
            std::mem::swap(scope, &mut scopes.next);
        }
    }

    /// Sends each player the transforms, from `updates`, of the entities it
    /// holds. Each player gets at most one batch.
    pub(crate) fn send_scoped_transforms(&self, updates: &[EntityUpdate]) {
        if updates.is_empty() {
            return;
        }
        let mut guard = self.scopes.borrow_mut();
        let scopes = &mut *guard;
        for (&player, scope) in &scopes.players {
            scopes.batch.clear();
            scopes.batch.extend(
                updates
                    .iter()
                    .filter(|update| scope.binary_search(&update.0).is_ok())
                    .copied(),
            );
            if scopes.batch.is_empty() {
                continue;
            }
            self.with_server(|server| {
                if let Err(err) = server.send_entity_transforms(player, &scopes.batch) {
                    warn!("cannot send the entity transforms to player {player}: {err}");
                }
            });
        }
    }

    /// Sends each player the state updates, from `updates`, of the entities it
    /// holds. Each player gets at most one batch.
    pub(crate) fn send_scoped_states(&self, updates: &[EntityStateUpdate]) {
        if updates.is_empty() {
            return;
        }
        let mut guard = self.scopes.borrow_mut();
        let scopes = &mut *guard;
        for (&player, scope) in &scopes.players {
            scopes.state_batch.clear();
            scopes.state_batch.extend(
                updates
                    .iter()
                    .filter(|update| scope.binary_search(&update.id).is_ok())
                    .copied(),
            );
            if scopes.state_batch.is_empty() {
                continue;
            }
            self.with_server(|server| {
                if let Err(err) = server.send_entity_states(player, &scopes.state_batch) {
                    warn!("cannot send the entity states to player {player}: {err}");
                }
            });
        }
    }

    /// Sends each player the state updates, from `updates`, of the entities of
    /// its bucket. Used for scoping off while buckets are in use. Each player
    /// gets at most one batch.
    pub(crate) fn send_bucketed_states(&self, updates: &[EntityStateUpdate]) {
        let entities = self.entities.borrow();
        let mut guard = self.scopes.borrow_mut();
        let scopes = &mut *guard;
        for &player in scopes.players.keys() {
            let bucket = bucket_in(&scopes.buckets, player);
            scopes.state_batch.clear();
            scopes.state_batch.extend(
                updates
                    .iter()
                    .filter(|update| entities.in_bucket(update.id, bucket))
                    .copied(),
            );
            if scopes.state_batch.is_empty() {
                continue;
            }
            self.with_server(|server| {
                if let Err(err) = server.send_entity_states(player, &scopes.state_batch) {
                    warn!("cannot send the entity states to player {player}: {err}");
                }
            });
        }
    }

    /// Sends each player the transforms, from `updates`, of the entities of its
    /// bucket. Used for scoping off while buckets are in use. Each player gets
    /// at most one batch.
    pub(crate) fn send_bucketed_transforms(&self, updates: &[EntityUpdate]) {
        let entities = self.entities.borrow();
        let mut guard = self.scopes.borrow_mut();
        let scopes = &mut *guard;
        for &player in scopes.players.keys() {
            let bucket = bucket_in(&scopes.buckets, player);
            scopes.batch.clear();
            scopes.batch.extend(
                updates
                    .iter()
                    .filter(|update| entities.in_bucket(update.0, bucket))
                    .copied(),
            );
            if scopes.batch.is_empty() {
                continue;
            }
            self.with_server(|server| {
                if let Err(err) = server.send_entity_transforms(player, &scopes.batch) {
                    warn!("cannot send the entity transforms to player {player}: {err}");
                }
            });
        }
    }

    /// Applies a change of routing buckets, then brings every client up to date.
    ///
    /// With scoping on, the scopes are recomputed and only the entities that
    /// entered or left each scope are sent. With scoping off, every client holds
    /// the entities of its bucket, so the held entities are listed before and
    /// after the change and the differences are sent. Buckets change rarely,
    /// so this may scan every player and entity.
    pub(crate) fn rebucket(&self, change: impl FnOnce(&mut EntityStore, &mut Scopes)) {
        if self.scoping() {
            {
                let mut entities = self.entities.borrow_mut();
                let mut scopes = self.scopes.borrow_mut();
                change(&mut entities, &mut scopes);
            }
            self.refresh_scopes();
            return;
        }
        let before = self.held_by_players();
        {
            let mut entities = self.entities.borrow_mut();
            let mut scopes = self.scopes.borrow_mut();
            change(&mut entities, &mut scopes);
        }
        let after = self.held_by_players();
        let entities = self.entities.borrow();
        let (mut entered, mut left) = (Vec::new(), Vec::new());
        for ((player, old), (_, new)) in before.iter().zip(&after) {
            diff(old, new, &mut entered, &mut left);
            for &id in &entered {
                self.send_entity_to(*player, id, &entities);
            }
            for &id in &left {
                self.send_removed_to(*player, id);
            }
        }
    }

    /// Each connected player with the entities it holds while scoping is off:
    /// the entities of its bucket, in ascending order.
    fn held_by_players(&self) -> Vec<(u32, Vec<u32>)> {
        let entities = self.entities.borrow();
        let scopes = self.scopes.borrow();
        scopes
            .players
            .keys()
            .map(|&player| {
                let bucket = bucket_in(&scopes.buckets, player);
                let held = entities
                    .ids_ascending()
                    .filter(|&id| entities.in_bucket(id, bucket))
                    .collect();
                (player, held)
            })
            .collect()
    }

    /// Sends `player` the entity `id` as it is now, then every replicated key
    /// of its state bag.
    fn send_entity_to(&self, player: u32, id: u32, entities: &EntityStore) {
        let Some((info, state, epoch)) = entities.info_state_and_epoch(id) else {
            return;
        };
        let entry = EntityEntry::from((id, info, state, epoch));
        let bag = entity_bag(id);
        let keys = self.state.borrow().replicated_in(&bag);
        self.with_server(|server| {
            if let Err(err) = server.send_entity_created(player, entry) {
                warn!("cannot send entity {id} to player {player}: {err}");
            }
            for (key, value) in &keys {
                if let Err(err) = server.send_state(player, &bag, key, value) {
                    warn!("cannot send state bag `{bag}` to player {player}: {err}");
                }
            }
        });
    }

    /// Tells `player` that the state bag of entity `id` was removed, and that
    /// the entity was deleted.
    fn send_removed_to(&self, player: u32, id: u32) {
        let bag = entity_bag(id);
        self.with_server(|server| {
            if let Err(err) = server.send_state_removed(player, &bag) {
                warn!("cannot send the removal of state bag `{bag}` to player {player}: {err}");
            }
            if let Err(err) = server.send_entity_deleted(player, id) {
                warn!("cannot send the deletion of entity {id} to player {player}: {err}");
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use sixm_core::EntityKind;

    use super::*;

    fn near(grid: &Grid, at: [f32; 3]) -> Vec<u32> {
        let mut found = Vec::new();
        grid.for_each_near(at, |id, _| found.push(id));
        found.sort_unstable();
        found
    }

    fn info(owner: Option<u32>, position: [f32; 3]) -> EntityInfo {
        EntityInfo {
            kind: EntityKind::Ped,
            model: 1,
            position,
            rotation: [0.0; 3],
            owner,
        }
    }

    #[test]
    fn the_nine_cells_around_a_point_are_searched() {
        let mut grid = Grid::new(10.0);
        grid.insert(1, [5.0, 5.0, 0.0]);
        grid.insert(2, [-5.0, 15.0, 0.0]);
        grid.insert(3, [25.0, 5.0, 0.0]);
        assert_eq!(near(&grid, [5.0, 5.0, 0.0]), vec![1, 2]);
    }

    #[test]
    fn a_move_within_a_cell_keeps_the_entity_where_it_is() {
        let mut grid = Grid::new(10.0);
        grid.insert(1, [1.0, 1.0, 0.0]);
        grid.move_to(1, [2.0, 2.0, 0.0]);
        assert_eq!(near(&grid, [2.0, 2.0, 0.0]), vec![1]);
        assert_eq!(grid.cells.len(), 1);
        let mut position = None;
        grid.for_each_near([2.0, 2.0, 0.0], |_, at| position = Some(at));
        assert_eq!(position, Some([2.0, 2.0, 0.0]));
    }

    #[test]
    fn a_move_across_a_cell_border_changes_the_cell() {
        let mut grid = Grid::new(10.0);
        grid.insert(1, [1.0, 1.0, 0.0]);
        grid.move_to(1, [25.0, 5.0, 0.0]);
        assert_eq!(near(&grid, [1.0, 1.0, 0.0]), Vec::<u32>::new());
        assert_eq!(near(&grid, [25.0, 5.0, 0.0]), vec![1]);
    }

    #[test]
    fn removing_an_entity_fixes_the_index_of_the_one_that_takes_its_place() {
        let mut grid = Grid::new(10.0);
        grid.insert(1, [1.0, 1.0, 0.0]);
        grid.insert(2, [2.0, 2.0, 0.0]);
        grid.insert(3, [3.0, 3.0, 0.0]);
        grid.remove(1);
        assert_eq!(near(&grid, [2.0, 2.0, 0.0]), vec![2, 3]);
        grid.remove(3);
        assert_eq!(near(&grid, [2.0, 2.0, 0.0]), vec![2]);
        grid.move_to(2, [50.0, 50.0, 0.0]);
        assert_eq!(near(&grid, [2.0, 2.0, 0.0]), Vec::<u32>::new());
        grid.remove(2);
        assert!(grid.cells.is_empty() && grid.locations.is_empty());
    }

    #[test]
    fn moving_or_removing_an_unknown_entity_does_nothing() {
        let mut grid = Grid::new(10.0);
        grid.move_to(7, [1.0, 1.0, 0.0]);
        grid.remove(7);
        assert!(grid.cells.is_empty());
    }

    #[test]
    fn the_diff_lists_entries_and_exits_in_order() {
        let (mut entered, mut left) = (Vec::new(), Vec::new());
        diff(&[1, 3, 5], &[2, 3, 6], &mut entered, &mut left);
        assert_eq!(entered, vec![2, 6]);
        assert_eq!(left, vec![1, 5]);

        diff(&[1, 2], &[1, 2], &mut entered, &mut left);
        assert!(entered.is_empty() && left.is_empty(), "no change");

        diff(&[], &[4, 9], &mut entered, &mut left);
        assert_eq!(entered, vec![4, 9]);
        assert_eq!(left, Vec::<u32>::new());

        diff(&[4, 9], &[], &mut entered, &mut left);
        assert_eq!(left, vec![4, 9]);
        assert_eq!(entered, Vec::<u32>::new());
    }

    #[test]
    fn owned_entities_are_in_scope_wherever_they_are() {
        let mut store = EntityStore::default();
        store.set_grid(Some(50.0));
        let ped = store.insert(info(Some(1), [0.0; 3])).unwrap();
        let far = store.insert(info(Some(1), [1000.0, 0.0, 0.0])).unwrap();
        let _other = store.insert(info(Some(2), [500.0, 0.0, 0.0])).unwrap();
        let mut scope = Vec::new();
        store.scope_into(1, store.position(ped), 50.0, 0, &mut scope);
        assert_eq!(scope, vec![ped, far]);
    }

    #[test]
    fn a_player_without_a_ped_holds_only_its_owned_entities() {
        let mut store = EntityStore::default();
        store.set_grid(Some(50.0));
        let vehicle = store.insert(info(None, [0.0; 3])).unwrap();
        let ped = store.insert(info(Some(1), [900.0, 0.0, 0.0])).unwrap();
        let mut scope = Vec::new();
        store.scope_into(1, None, 50.0, 0, &mut scope);
        assert_eq!(
            scope,
            vec![ped],
            "the vehicle is near nobody, and the ped is owned"
        );
        assert!(!scope.contains(&vehicle));
    }

    #[test]
    fn the_radius_is_measured_in_three_dimensions_and_includes_its_edge() {
        let mut store = EntityStore::default();
        store.set_grid(Some(50.0));
        let ped = store.insert(info(None, [0.0; 3])).unwrap();
        let edge = store.insert(info(None, [50.0, 0.0, 0.0])).unwrap();
        let diagonal = store.insert(info(None, [30.0, 30.0, 0.0])).unwrap();
        let high = store.insert(info(None, [30.0, 30.0, 40.0])).unwrap();
        let mut scope = Vec::new();
        store.scope_into(9, Some([0.0; 3]), 50.0, 0, &mut scope);
        assert_eq!(
            scope,
            vec![ped, edge, diagonal],
            "58 metres away in 3D is outside, the others are inside"
        );
        assert!(!scope.contains(&high));
    }

    #[test]
    fn a_moved_entity_is_found_at_its_new_place() {
        let mut store = EntityStore::default();
        store.set_grid(Some(50.0));
        let id = store.insert(info(None, [0.0; 3])).unwrap();
        store.set_position(id, [500.0, 0.0, 0.0]).unwrap();
        let mut scope = Vec::new();
        store.scope_into(1, Some([0.0; 3]), 50.0, 0, &mut scope);
        assert_eq!(scope, Vec::<u32>::new());
        store.scope_into(1, Some([500.0, 0.0, 0.0]), 50.0, 0, &mut scope);
        assert_eq!(scope, vec![id]);
    }
}
