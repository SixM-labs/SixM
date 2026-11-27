//! Entities as the wire carries them.

use serde::{Deserialize, Serialize};
use sixm_core::{EntityAttachment, EntityInfo, EntityKind, EntityState};

use crate::error::NetError;

/// One entity transform as the wire carries it: `(id, position, rotation,
/// epoch)`. A part is `None` when it did not change, and at least one part is
/// present in every transform that is sent.
pub type EntityTransform = (u32, Option<[f32; 3]>, Option<[f32; 3]>, u32);

/// Bitmask bit in an [`EntityStateUpdate`]: `frozen` changed.
pub const STATE_FROZEN: u8 = 1 << 0;
/// Bitmask bit in an [`EntityStateUpdate`]: `velocity` changed.
pub const STATE_VELOCITY: u8 = 1 << 1;
/// Bitmask bit in an [`EntityStateUpdate`]: `health` changed.
pub const STATE_HEALTH: u8 = 1 << 2;
/// Bitmask bit in an [`EntityStateUpdate`]: `max_health` changed.
pub const STATE_MAX_HEALTH: u8 = 1 << 3;
/// Bitmask bit in an [`EntityStateUpdate`]: `visible` changed.
pub const STATE_VISIBLE: u8 = 1 << 4;
/// Bitmask bit in an [`EntityStateUpdate`]: `alpha` changed.
pub const STATE_ALPHA: u8 = 1 << 5;
/// Bitmask bit in an [`EntityStateUpdate`]: `attachment` changed.
pub const STATE_ATTACHMENT: u8 = 1 << 6;
/// Valid bitmask bits for [`EntityStateUpdate`].
pub(crate) const STATE_FLAGS: u8 = STATE_FROZEN
    | STATE_VELOCITY
    | STATE_HEALTH
    | STATE_MAX_HEALTH
    | STATE_VISIBLE
    | STATE_ALPHA
    | STATE_ATTACHMENT;

/// Entity attachment as the wire carries it.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct EntityAttachmentEntry {
    /// Parent entity id.
    pub parent: u32,
    /// Bone index on the parent.
    pub bone_index: i32,
    /// Local offset in metres.
    pub offset: [f32; 3],
    /// Local rotation in degrees: pitch, roll, yaw.
    pub rotation: [f32; 3],
}

impl From<EntityAttachment> for EntityAttachmentEntry {
    fn from(att: EntityAttachment) -> Self {
        Self {
            parent: att.parent,
            bone_index: att.bone_index,
            offset: att.offset,
            rotation: att.rotation,
        }
    }
}

impl From<EntityAttachmentEntry> for EntityAttachment {
    fn from(att: EntityAttachmentEntry) -> Self {
        Self {
            parent: att.parent,
            bone_index: att.bone_index,
            offset: att.offset,
            rotation: att.rotation,
        }
    }
}

/// Replicated per-entity state as carried in [`EntityEntry`].
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct EntityStateEntry {
    pub frozen: bool,
    pub velocity: [f32; 3],
    pub health: i32,
    pub max_health: i32,
    pub visible: bool,
    pub alpha: u8,
    pub attachment: Option<EntityAttachmentEntry>,
}

impl Default for EntityStateEntry {
    fn default() -> Self {
        Self::from(EntityState::default())
    }
}

impl From<EntityState> for EntityStateEntry {
    fn from(state: EntityState) -> Self {
        Self {
            frozen: state.frozen,
            velocity: state.velocity,
            health: state.health,
            max_health: state.max_health,
            visible: state.visible,
            alpha: state.alpha,
            attachment: state.attachment.map(EntityAttachmentEntry::from),
        }
    }
}

impl From<EntityStateEntry> for EntityState {
    fn from(entry: EntityStateEntry) -> Self {
        Self {
            frozen: entry.frozen,
            velocity: entry.velocity,
            health: entry.health,
            max_health: entry.max_health,
            visible: entry.visible,
            alpha: entry.alpha,
            attachment: entry.attachment.map(EntityAttachment::from),
        }
    }
}

/// Delta update of an entity's replicated state for one frame. Only fields
/// that changed since the last frame are `Some`, and at least one field must
/// be `Some` when sent.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct EntityStateUpdate {
    pub id: u32,
    pub frozen: Option<bool>,
    pub velocity: Option<[f32; 3]>,
    pub health: Option<i32>,
    pub max_health: Option<i32>,
    pub visible: Option<bool>,
    pub alpha: Option<u8>,
    /// `None` = unchanged, `Some(None)` = detached, `Some(Some(att))` = attached.
    pub attachment: Option<Option<EntityAttachmentEntry>>,
}

impl EntityStateUpdate {
    /// Bitmask of fields present in this delta update.
    pub fn flags(&self) -> u8 {
        self.frozen.map_or(0, |_| STATE_FROZEN)
            | self.velocity.map_or(0, |_| STATE_VELOCITY)
            | self.health.map_or(0, |_| STATE_HEALTH)
            | self.max_health.map_or(0, |_| STATE_MAX_HEALTH)
            | self.visible.map_or(0, |_| STATE_VISIBLE)
            | self.alpha.map_or(0, |_| STATE_ALPHA)
            | self.attachment.map_or(0, |_| STATE_ATTACHMENT)
    }
}

impl Serialize for EntityStateUpdate {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeTuple;
        let flags = self.flags();
        if flags == 0 {
            return Err(serde::ser::Error::custom(
                "an entity state update carries at least one changed field",
            ));
        }
        let len = 2 + usize::try_from(flags.count_ones()).unwrap_or(0);
        let mut tup = serializer.serialize_tuple(len)?;
        tup.serialize_element(&self.id)?;
        tup.serialize_element(&flags)?;
        if let Some(frozen) = self.frozen {
            tup.serialize_element(&frozen)?;
        }
        if let Some(velocity) = self.velocity {
            tup.serialize_element(&velocity)?;
        }
        if let Some(health) = self.health {
            tup.serialize_element(&health)?;
        }
        if let Some(max_health) = self.max_health {
            tup.serialize_element(&max_health)?;
        }
        if let Some(visible) = self.visible {
            tup.serialize_element(&visible)?;
        }
        if let Some(alpha) = self.alpha {
            tup.serialize_element(&alpha)?;
        }
        if let Some(attachment) = self.attachment {
            tup.serialize_element(&attachment)?;
        }
        tup.end()
    }
}

/// One entity as the wire carries it: its id, transform, owner, epoch and state.
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
    /// Replicated state of the entity.
    pub state: EntityStateEntry,
}

impl From<(u32, EntityInfo, EntityState, u32)> for EntityEntry {
    fn from((id, info, state, epoch): (u32, EntityInfo, EntityState, u32)) -> Self {
        Self {
            id,
            kind: info.kind.code(),
            model: info.model,
            position: info.position,
            rotation: info.rotation,
            owner: info.owner.unwrap_or(0),
            epoch,
            state: EntityStateEntry::from(state),
        }
    }
}

impl TryFrom<EntityEntry> for (u32, EntityInfo, EntityState) {
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
        Ok((entry.id, info, EntityState::from(entry.state)))
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
            state: EntityStateEntry::default(),
        };
        let (id, info, state) =
            <(u32, EntityInfo, EntityState)>::try_from(entry).expect("a known kind");
        assert_eq!(
            (id, info.owner, state),
            (2, Some(4), EntityState::default())
        );
        let server_owned = EntityInfo {
            owner: None,
            ..info
        };
        let back = EntityEntry::from((id, server_owned, state, 3));
        assert_eq!((back.owner, back.epoch), (0, 3));
    }
}
