//! Networked entities as the core sees them: what an entity is, independent of
//! any game.
//!
//! The server owns every entity. It creates, moves and deletes them, and every
//! client keeps a copy. A game adapter will later turn that copy into game
//! objects; until then an entity is just its kind, model and transform.

/// Most entities that exist at once on a server.
pub const MAX_ENTITIES: usize = 65_536;

/// Default health and max health for newly created entities (peds, vehicles, objects).
pub const DEFAULT_ENTITY_HEALTH: i32 = 200;

/// Attachment of an entity to a parent entity.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct EntityAttachment {
    /// Id of the parent entity this entity is attached to.
    pub parent: u32,
    /// Bone index on the parent entity.
    pub bone_index: i32,
    /// Local position offset from the parent in metres.
    pub offset: [f32; 3],
    /// Local rotation offset relative to the parent in degrees: pitch, roll, yaw.
    pub rotation: [f32; 3],
}

/// Replicated per-entity state beyond transform and ownership.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct EntityState {
    /// Whether the entity's position is frozen. Frozen entities refuse owner
    /// transform updates.
    pub frozen: bool,
    /// Linear velocity in metres per second.
    pub velocity: [f32; 3],
    /// Current health.
    pub health: i32,
    /// Maximum health.
    pub max_health: i32,
    /// Whether the entity is visible.
    pub visible: bool,
    /// Opacity from `0` (transparent) to `255` (opaque).
    pub alpha: u8,
    /// Attachment to a parent entity, or `None` when detached.
    pub attachment: Option<EntityAttachment>,
}

impl Default for EntityState {
    fn default() -> Self {
        Self {
            frozen: false,
            velocity: [0.0; 3],
            health: DEFAULT_ENTITY_HEALTH,
            max_health: DEFAULT_ENTITY_HEALTH,
            visible: true,
            alpha: 255,
            attachment: None,
        }
    }
}

/// What kind of game object an entity is. The codes are the ones
/// `GetEntityType` returns.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum EntityKind {
    Ped,
    Vehicle,
    Object,
}

impl EntityKind {
    /// The `GetEntityType` code: 1 for a ped, 2 for a vehicle, 3 for an object.
    pub fn code(self) -> u8 {
        match self {
            Self::Ped => 1,
            Self::Vehicle => 2,
            Self::Object => 3,
        }
    }

    /// The kind with `GetEntityType` code `code`, or `None` for any other code.
    pub fn from_code(code: u8) -> Option<Self> {
        match code {
            1 => Some(Self::Ped),
            2 => Some(Self::Vehicle),
            3 => Some(Self::Object),
            _ => None,
        }
    }
}

/// One entity: its kind, model and transform.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct EntityInfo {
    pub kind: EntityKind,
    /// Model name hash, as `GetHashKey` computes it.
    pub model: u32,
    /// World position in metres.
    pub position: [f32; 3],
    /// Rotation in degrees: pitch around x, roll around y, yaw around z. The
    /// heading is the yaw.
    pub rotation: [f32; 3],
    /// The player whose client moves the entity, or `None` when only the
    /// server does. A player's own ped is owned by that player.
    pub owner: Option<u32>,
}

/// Name of the state bag of entity `entity`.
pub fn entity_bag(entity: u32) -> String {
    format!("entity:{entity}")
}

/// The entity a bag belongs to, or `None` when `bag` is not an entity bag.
pub fn entity_of_bag(bag: &str) -> Option<u32> {
    let digits = bag.strip_prefix("entity:")?;
    let entity: u32 = digits.parse().ok()?;
    // Only the canonical spelling names a bag, so `entity:05` is not `entity:5`.
    (entity.to_string() == digits).then_some(entity)
}

/// Whether every component is finite, which every stored transform must be.
pub fn is_finite(components: [f32; 3]) -> bool {
    components.iter().all(|component| component.is_finite())
}

/// Converts entity Euler angles `[pitch, roll, yaw]` in degrees (`Rz(yaw) * Rx(pitch) * Ry(roll)`)
/// into a unit quaternion in `[w, x, y, z]` order.
#[expect(
    clippy::cast_possible_truncation,
    reason = "entity transforms are stored as f32"
)]
pub fn euler_to_quat(rotation: [f32; 3]) -> [f32; 4] {
    let half_pitch = f64::from(rotation[0]).to_radians() * 0.5;
    let half_roll = f64::from(rotation[1]).to_radians() * 0.5;
    let half_yaw = f64::from(rotation[2]).to_radians() * 0.5;

    let (sx, cx) = half_pitch.sin_cos();
    let (sy, cy) = half_roll.sin_cos();
    let (sz, cz) = half_yaw.sin_cos();

    let w = cz * cx * cy - sz * sx * sy;
    let x = cz * sx * cy - sz * cx * sy;
    let y = cz * cx * sy + sz * sx * cy;
    let z = sz * cx * cy + cz * sx * sy;

    [w as f32, x as f32, y as f32, z as f32]
}

/// Converts a quaternion in `[w, x, y, z]` order into entity Euler angles
/// `[pitch, roll, yaw]` in degrees matching [`euler_to_quat`].
#[expect(
    clippy::cast_possible_truncation,
    reason = "entity transforms are stored as f32"
)]
pub fn quat_to_euler(quat: [f32; 4]) -> [f32; 3] {
    let [w, x, y, z] = quat.map(f64::from);
    let norm = (w * w + x * x + y * y + z * z).sqrt();
    if norm <= 1e-12 || !norm.is_finite() {
        return [0.0; 3];
    }
    let (w, x, y, z) = (w / norm, x / norm, y / norm, z / norm);

    let sin_pitch = (2.0 * (y * z + w * x)).clamp(-1.0, 1.0);
    let pitch = sin_pitch.asin();

    let (roll, yaw) = if (1.0 - sin_pitch.abs()) < 1e-6 {
        let yaw = (2.0 * (x * y + w * z)).atan2(1.0 - 2.0 * (y * y + z * z));
        (0.0, yaw)
    } else {
        let roll = (2.0 * (w * y - x * z)).atan2(1.0 - 2.0 * (x * x + y * y));
        let yaw = (2.0 * (w * z - x * y)).atan2(1.0 - 2.0 * (x * x + z * z));
        (roll, yaw)
    };

    [
        pitch.to_degrees() as f32,
        roll.to_degrees() as f32,
        yaw.to_degrees() as f32,
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kind_codes_round_trip() {
        for kind in [EntityKind::Ped, EntityKind::Vehicle, EntityKind::Object] {
            assert_eq!(EntityKind::from_code(kind.code()), Some(kind));
        }
        assert_eq!(EntityKind::from_code(0), None);
        assert_eq!(EntityKind::from_code(4), None);
    }

    #[test]
    fn entity_bags_round_trip_and_only_canonical_names_count() {
        assert_eq!(entity_bag(12), "entity:12");
        assert_eq!(entity_of_bag("entity:12"), Some(12));
        for bag in ["entity:012", "entity:", "entity:-1", "entity:x", "player:1"] {
            assert_eq!(entity_of_bag(bag), None, "{bag}");
        }
    }

    #[test]
    fn non_finite_components_are_detected() {
        assert!(is_finite([0.0, -1.5, 1e30]));
        assert!(!is_finite([f32::NAN, 0.0, 0.0]));
        assert!(!is_finite([0.0, f32::INFINITY, 0.0]));
    }

    #[test]
    fn euler_and_quat_round_trip() {
        let cases = [
            [0.0, 0.0, 0.0],
            [0.0, 0.0, 90.0],
            [25.0, -15.0, 130.0],
            [-40.0, 10.0, -65.0],
        ];
        for euler in cases {
            let q = euler_to_quat(euler);
            let back = quat_to_euler(q);
            for (got, expected) in back.into_iter().zip(euler) {
                assert!(
                    (got - expected).abs() < 1e-3,
                    "{euler:?} -> {q:?} -> {back:?}"
                );
            }
        }
        assert_eq!(euler_to_quat([0.0, 0.0, 0.0]), [1.0, 0.0, 0.0, 0.0]);
        assert_eq!(quat_to_euler([0.0, 0.0, 0.0, 0.0]), [0.0, 0.0, 0.0]);
    }

    #[test]
    fn default_entity_state_has_expected_values() {
        let state = EntityState::default();
        assert!(!state.frozen);
        assert_eq!(state.velocity, [0.0; 3]);
        assert_eq!(state.health, DEFAULT_ENTITY_HEALTH);
        assert_eq!(state.max_health, DEFAULT_ENTITY_HEALTH);
        assert!(state.visible);
        assert_eq!(state.alpha, 255);
        assert_eq!(state.attachment, None);
    }
}
