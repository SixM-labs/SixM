//! Entities as the wire carries them.

use serde::{Deserialize, Serialize};
use sixm_core::{EntityInfo, EntityKind};

use crate::error::NetError;

/// One entity as the wire carries it: its id and its state.
///
/// The id is also the entity's network id. `kind` is the `GetEntityType` code
/// of the entity: 1 for a ped, 2 for a vehicle and 3 for an object.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct EntityEntry {
    /// Id of the entity, which is also its network id.
    pub id: u32,
    /// The `GetEntityType` code of the kind.
    pub kind: u8,
    /// Model name hash, as `GetHashKey` computes it.
    pub model: u32,
    /// World position in metres.
    pub position: [f32; 3],
    /// Rotation in degrees: pitch, roll and yaw.
    pub rotation: [f32; 3],
}

impl From<(u32, EntityInfo)> for EntityEntry {
    fn from((id, info): (u32, EntityInfo)) -> Self {
        Self {
            id,
            kind: info.kind.code(),
            model: info.model,
            position: info.position,
            rotation: info.rotation,
        }
    }
}

impl TryFrom<EntityEntry> for (u32, EntityInfo) {
    type Error = NetError;

    /// Fails when the entry's kind code is not one of the known kinds.
    fn try_from(entry: EntityEntry) -> Result<Self, NetError> {
        let kind = EntityKind::from_code(entry.kind).ok_or_else(|| {
            NetError::InvalidInput(format!(
                "entity {} has unknown kind {}",
                entry.id, entry.kind
            ))
        })?;
        let info = EntityInfo {
            kind,
            model: entry.model,
            position: entry.position,
            rotation: entry.rotation,
        };
        Ok((entry.id, info))
    }
}
