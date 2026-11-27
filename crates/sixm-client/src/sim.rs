//! Phase 1 simulated [`GameAdapter`]: renders nothing, integrates velocity
//! and scripted movement for mirrored entities, and registers sample adapter
//! natives (`GetEntitySpeed`, `TaskGoStraightToCoord`, `GetSimEntityCoords`).

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::rc::Rc;
use std::time::Instant;

use sixm_core::{
    AdapterError, AdapterSink, EntityInfo, EntityState, GameAdapter, MirrorChange, NativeRegistry,
    ScriptError, Value, is_finite,
};

/// Upper bound for one integration step so stalls never teleport entities across the map.
const MAX_DT_SECS: f32 = 0.25;
/// Distance threshold under which `TaskGoStraightToCoord` finishes at the target point.
const ARRIVAL_EPSILON: f32 = 1e-4;

#[derive(Debug, Clone, Copy)]
struct SimEntity {
    info: EntityInfo,
    state: EntityState,
    dirty_owned: bool,
    scripted_target: Option<([f32; 3], f32)>,
}

#[derive(Debug, Default)]
struct SimWorld {
    entities: BTreeMap<u32, SimEntity>,
    local_player: Option<u32>,
    last_tick: Option<Instant>,
}

/// Deterministic, dependency-free [`GameAdapter`] for tests, bot clients, and `--sim`.
#[derive(Debug)]
pub struct SimAdapter {
    world: Rc<RefCell<SimWorld>>,
    sink: Option<AdapterSink>,
    natives: NativeRegistry,
}

impl SimAdapter {
    /// Creates a new `SimAdapter` with its sample natives registered.
    pub fn new() -> Self {
        let world = Rc::new(RefCell::new(SimWorld::default()));
        let mut natives = NativeRegistry::default();
        register_natives(&mut natives, &world);
        Self {
            world,
            sink: None,
            natives,
        }
    }

    /// Snapshot of an entity inside the simulated world, if present.
    pub fn entity(&self, id: u32) -> Option<EntityInfo> {
        self.world
            .borrow()
            .entities
            .get(&id)
            .map(|entry| entry.info)
    }

    fn step(&mut self, now: Instant) {
        let mut world = self.world.borrow_mut();
        let dt = match world.last_tick {
            Some(prev) => now
                .saturating_duration_since(prev)
                .as_secs_f32()
                .min(MAX_DT_SECS),
            None => 0.0,
        };
        world.last_tick = Some(now);
        if dt <= 0.0 {
            return;
        }
        let local_player = world.local_player;
        for entry in world.entities.values_mut() {
            if entry.state.frozen || entry.state.attachment.is_some() {
                continue;
            }
            let owned = local_player.is_some() && entry.info.owner == local_player;
            if owned && let Some((target, speed)) = entry.scripted_target {
                let arrived = advance_toward(
                    &mut entry.info.position,
                    &mut entry.state.velocity,
                    target,
                    speed,
                    dt,
                );
                if arrived {
                    entry.scripted_target = None;
                }
                entry.dirty_owned = true;
            } else if entry.state.velocity.iter().any(|&v| v != 0.0) {
                for (pos, vel) in entry.info.position.iter_mut().zip(entry.state.velocity) {
                    *pos += vel * dt;
                }
                if owned {
                    entry.dirty_owned = true;
                }
            }
        }
    }
}

impl Default for SimAdapter {
    fn default() -> Self {
        Self::new()
    }
}

impl GameAdapter for SimAdapter {
    fn attach(&mut self, sink: AdapterSink) -> Result<(), AdapterError> {
        self.world.borrow_mut().local_player = sink.local_player();
        self.sink = Some(sink);
        Ok(())
    }

    fn detach(&mut self) {
        let mut world = self.world.borrow_mut();
        world.entities.clear();
        world.local_player = None;
        world.last_tick = None;
        self.sink = None;
    }

    fn before_scripts(&mut self, _now: Instant) {}

    fn after_scripts(&mut self, now: Instant) {
        self.step(now);
    }

    fn apply_changes(&mut self, changes: &[MirrorChange], _errors: &mut Vec<(u32, AdapterError)>) {
        let mut world = self.world.borrow_mut();
        let local_player = world.local_player;
        for &change in changes {
            match change {
                MirrorChange::Created { id, info } => {
                    world.entities.insert(
                        id,
                        SimEntity {
                            info,
                            state: EntityState::default(),
                            dirty_owned: false,
                            scripted_target: None,
                        },
                    );
                }
                MirrorChange::Moved { id, info, teleport } => {
                    if let Some(entry) = world.entities.get_mut(&id) {
                        entry.info.position = info.position;
                        entry.info.rotation = info.rotation;
                        entry.dirty_owned = false;
                        if teleport {
                            entry.scripted_target = None;
                        }
                    }
                }
                MirrorChange::OwnerChanged { id, info } => {
                    if let Some(entry) = world.entities.get_mut(&id) {
                        entry.info = info;
                        entry.dirty_owned = false;
                        if info.owner != local_player {
                            entry.scripted_target = None;
                        }
                    }
                }
                MirrorChange::StateChanged { id, state } => {
                    if let Some(entry) = world.entities.get_mut(&id) {
                        entry.state = state;
                    }
                }
                MirrorChange::Removed { id } => {
                    world.entities.remove(&id);
                }
            }
        }
    }

    fn read_owned(&mut self, owned: &[u32], out: &mut Vec<(u32, [f32; 3], [f32; 3])>) {
        let mut world = self.world.borrow_mut();
        for &id in owned {
            if let Some(entry) = world.entities.get_mut(&id)
                && entry.dirty_owned
            {
                entry.dirty_owned = false;
                out.push((id, entry.info.position, entry.info.rotation));
            }
        }
    }

    fn natives(&self) -> &NativeRegistry {
        &self.natives
    }
}

fn advance_toward(
    pos: &mut [f32; 3],
    vel: &mut [f32; 3],
    target: [f32; 3],
    speed: f32,
    dt: f32,
) -> bool {
    let dx = target[0] - pos[0];
    let dy = target[1] - pos[1];
    let dz = target[2] - pos[2];
    let dist_sq = dx * dx + dy * dy + dz * dz;
    if dist_sq <= ARRIVAL_EPSILON * ARRIVAL_EPSILON || speed <= 0.0 {
        *pos = target;
        *vel = [0.0; 3];
        return true;
    }
    let dist = dist_sq.sqrt();
    let step = speed * dt;
    if step >= dist {
        *pos = target;
        *vel = [0.0; 3];
        true
    } else {
        let scale = speed / dist;
        *vel = [dx * scale, dy * scale, dz * scale];
        pos[0] += vel[0] * dt;
        pos[1] += vel[1] * dt;
        pos[2] += vel[2] * dt;
        false
    }
}

fn register_natives(natives: &mut NativeRegistry, world: &Rc<RefCell<SimWorld>>) {
    let w = Rc::clone(world);
    let _ = natives.register("GetEntitySpeed", move |args| {
        let Some(id) = args.first().and_then(entity_id_arg) else {
            return Ok(vec![Value::Float(0.0)]);
        };
        let speed = w.borrow().entities.get(&id).map_or(0.0, |entry| {
            let [vx, vy, vz] = entry.state.velocity;
            f64::from((vx * vx + vy * vy + vz * vz).sqrt())
        });
        Ok(vec![Value::Float(speed)])
    });

    let w = Rc::clone(world);
    let _ = natives.register("GetSimEntityCoords", move |args| {
        let Some(id) = args.first().and_then(entity_id_arg) else {
            return Ok(vec![Value::Vector3([0.0; 3])]);
        };
        let pos = w
            .borrow()
            .entities
            .get(&id)
            .map_or([0.0; 3], |entry| entry.info.position);
        Ok(vec![Value::Vector3(pos)])
    });

    let w = Rc::clone(world);
    let _ = natives.register("TaskGoStraightToCoord", move |args| {
        let [entity_v, x_v, y_v, z_v, speed_v, ..] = args else {
            return Err(ScriptError::Runtime(
                "TaskGoStraightToCoord expects (entity, x, y, z, speed)".to_owned(),
            ));
        };
        let Some(id) = entity_id_arg(entity_v) else {
            return Ok(Vec::new());
        };
        let target = [
            float_arg(x_v, "x")?,
            float_arg(y_v, "y")?,
            float_arg(z_v, "z")?,
        ];
        if !is_finite(target) {
            return Err(ScriptError::Runtime(
                "TaskGoStraightToCoord: target coordinates must be finite".to_owned(),
            ));
        }
        let speed = float_arg(speed_v, "speed")?;
        if !speed.is_finite() || speed < 0.0 {
            return Err(ScriptError::Runtime(
                "TaskGoStraightToCoord: speed must be finite and non-negative".to_owned(),
            ));
        }
        if let Some(entry) = w.borrow_mut().entities.get_mut(&id) {
            entry.scripted_target = Some((target, speed));
        }
        Ok(Vec::new())
    });
}

fn entity_id_arg(value: &Value) -> Option<u32> {
    match value {
        Value::Int(i) => u32::try_from(*i).ok(),
        #[expect(
            clippy::cast_possible_truncation,
            clippy::cast_sign_loss,
            reason = "checked finite integer within 0..=u32::MAX"
        )]
        Value::Float(f)
            if f.is_finite() && f.fract() == 0.0 && (0.0..=f64::from(u32::MAX)).contains(f) =>
        {
            Some(*f as u32)
        }
        _ => None,
    }
}

fn float_arg(value: &Value, what: &str) -> Result<f32, ScriptError> {
    #[expect(
        clippy::cast_possible_truncation,
        clippy::cast_precision_loss,
        reason = "coordinate and speed components are 32-bit floats"
    )]
    match value {
        Value::Int(i) => Ok(*i as f32),
        Value::Float(f) if f.is_finite() => Ok(*f as f32),
        Value::Float(_) => Err(ScriptError::Runtime(format!(
            "TaskGoStraightToCoord: {what} must be finite"
        ))),
        _ => Err(ScriptError::Runtime(format!(
            "TaskGoStraightToCoord: {what} must be a number"
        ))),
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use sixm_core::{EntityKind, joaat};

    use super::*;

    fn test_entity(owner: Option<u32>, pos: [f32; 3]) -> EntityInfo {
        EntityInfo {
            kind: EntityKind::Vehicle,
            model: joaat(b"adder"),
            position: pos,
            rotation: [0.0; 3],
            owner,
        }
    }

    #[test]
    fn sim_adapter_moves_owned_entities_respects_freeze_and_teleport() {
        let mut sim = SimAdapter::new();
        sim.world.borrow_mut().local_player = Some(1);

        let mut errs = Vec::new();
        sim.apply_changes(
            &[MirrorChange::Created {
                id: 7,
                info: test_entity(Some(1), [0.0, 0.0, 0.0]),
            }],
            &mut errs,
        );
        assert!(errs.is_empty());

        let task = sim
            .natives()
            .get("TaskGoStraightToCoord")
            .expect("task native")
            .clone();
        task(&[
            Value::Int(7),
            Value::Float(10.0),
            Value::Float(0.0),
            Value::Float(0.0),
            Value::Float(10.0),
        ])
        .expect("schedule move");

        let t0 = Instant::now();
        sim.after_scripts(t0);
        let mut owned = Vec::new();
        sim.read_owned(&[7], &mut owned);
        assert_eq!(owned, Vec::new());

        sim.after_scripts(t0 + Duration::from_millis(200));
        sim.read_owned(&[7], &mut owned);
        assert_eq!(owned.len(), 1);
        assert!((owned[0].1[0] - 2.0).abs() < 1e-4);

        let speed = sim.natives().get("GetEntitySpeed").expect("speed native")(&[Value::Int(7)])
            .expect("speed call");
        assert!(matches!(speed[0], Value::Float(s) if (s - 10.0).abs() < 1e-4));

        // Freezing pauses further simulation steps.
        sim.apply_changes(
            &[MirrorChange::StateChanged {
                id: 7,
                state: EntityState {
                    frozen: true,
                    ..EntityState::default()
                },
            }],
            &mut errs,
        );
        sim.after_scripts(t0 + Duration::from_millis(400));
        owned.clear();
        sim.read_owned(&[7], &mut owned);
        assert_eq!(owned, Vec::new());

        // Teleport snaps position and clears scripted target.
        sim.apply_changes(
            &[MirrorChange::Moved {
                id: 7,
                info: test_entity(Some(1), [50.0, 0.0, 0.0]),
                teleport: true,
            }],
            &mut errs,
        );
        sim.apply_changes(
            &[MirrorChange::StateChanged {
                id: 7,
                state: EntityState::default(),
            }],
            &mut errs,
        );
        sim.after_scripts(t0 + Duration::from_millis(600));
        sim.read_owned(&[7], &mut owned);
        assert_eq!(owned, Vec::new());
        assert_eq!(sim.entity(7).expect("entity").position, [50.0, 0.0, 0.0]);
    }
}
