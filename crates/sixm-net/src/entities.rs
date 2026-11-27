//! Entities as the wire carries them.

use serde::{Deserialize, Serialize};
use sixm_core::{EntityInfo, EntityKind};

use crate::error::NetError;

/// One entity transform as the wire carries it: `(id, position, rotation,
/// epoch)`. A part is `None` when it did not change, and at least one part is
/// present in every transform that is sent.
pub type EntityTransform = (u32, Option<[f32; 3]>, Option<[f32; 3]>, u32);

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
    /// Id of the player whose client moves the entity, or 0 when only the
    /// server does. Player ids start at 1.
    pub owner: u32,
    /// Teleport epoch: how many times the server has moved the entity itself.
    /// An owner's update is accepted only for the epoch the server is at.
    pub epoch: u32,
}

impl From<(u32, EntityInfo, u32)> for EntityEntry {
    fn from((id, info, epoch): (u32, EntityInfo, u32)) -> Self {
        Self {
            id,
            kind: info.kind.code(),
            model: info.model,
            position: info.position,
            rotation: info.rotation,
            owner: info.owner.unwrap_or(0),
            epoch,
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
            owner: (entry.owner != 0).then_some(entry.owner),
        };
        Ok((entry.id, info))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn owner_zero_on_the_wire_means_no_owner() {
        let entry = EntityEntry {
            id: 2,
            kind: 1,
            model: 7,
            position: [0.0; 3],
            rotation: [0.0; 3],
            owner: 4,
            epoch: 3,
        };
        let (id, info) = <(u32, EntityInfo)>::try_from(entry).expect("a known kind");
        assert_eq!((id, info.owner), (2, Some(4)));
        let server_owned = EntityInfo {
            owner: None,
            ..info
        };
        let back = EntityEntry::from((id, server_owned, 3));
        assert_eq!((back.owner, back.epoch), (0, 3));
    }
}
