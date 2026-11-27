//! Networked entities as the core sees them: what an entity is, independent of
//! any game.
//!
//! The server owns every entity. It creates, moves and deletes them, and every
//! client keeps a copy. A game adapter will later turn that copy into game
//! objects; until then an entity is just its kind, model and transform.

/// Most entities that exist at once on a server.
pub const MAX_ENTITIES: usize = 65_536;

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
}
