//! Entity functions: the natives that read entities on both sides, the ones
//! that create, move and delete them on the server, the movement functions of
//! both sides, `GetGamePool` on the client, and `Entity(id)` for an entity's
//! state bag.
//!
//! Every read asks the host, so an unknown entity reads as the neutral value of
//! its type: a zero vector, zero, or `false`. A setter on an unknown entity does
//! nothing. A number that is not finite raises an error. On the client a setter
//! on an entity this client does not own does nothing either.

use std::rc::Weak;

use mlua::{Function, Lua, Table, Value as LuaValue};
use sixm_core::{
    EntityAttachment, EntityInfo, EntityKind, EntityState, entity_bag, entity_of_bag,
    euler_to_quat, joaat, quat_to_euler,
};

use crate::client::client;
use crate::players::{player_id, server, whole_number};
use crate::runtime::{LuaRuntime, bind_fn, lua_err};
use crate::state::state_bag;
use crate::vector::{Vector, number_of};

/// Installs the functions that both sides have.
pub(crate) fn install(lua: &Lua, weak: &Weak<LuaRuntime>, globals: &Table) -> mlua::Result<()> {
    let does_exist = bind_fn(lua, weak, |rt, (entity,): (LuaValue,)| {
        Ok(existing(rt, &entity)?.is_some())
    })?;
    let coords = bind_fn(lua, weak, |rt, (entity,): (LuaValue,)| {
        let position = existing(rt, &entity)?.map_or([0.0; 3], |(_, info)| info.position);
        rt.lua.create_userdata(Vector::new(position))
    })?;
    let heading = bind_fn(lua, weak, |rt, (entity,): (LuaValue,)| {
        Ok(existing(rt, &entity)?.map_or(0.0, |(_, info)| f64::from(info.rotation[2])))
    })?;
    let rotation = bind_fn(lua, weak, |rt, (entity, _order): (LuaValue, LuaValue)| {
        let rotation = existing(rt, &entity)?.map_or([0.0; 3], |(_, info)| info.rotation);
        rt.lua.create_userdata(Vector::new(rotation))
    })?;
    let quaternion = bind_fn(lua, weak, |rt, (entity,): (LuaValue,)| {
        let [w, x, y, z] =
            existing(rt, &entity)?.map_or([0.0; 4], |(_, info)| euler_to_quat(info.rotation));
        Ok((f64::from(x), f64::from(y), f64::from(z), f64::from(w)))
    })?;
    let model = bind_fn(lua, weak, |rt, (entity,): (LuaValue,)| {
        Ok(existing(rt, &entity)?.map_or(0, |(_, info)| signed_hash(info.model)))
    })?;
    let entity_type = bind_fn(lua, weak, |rt, (entity,): (LuaValue,)| {
        Ok(existing(rt, &entity)?.map_or(0, |(_, info)| i64::from(info.kind.code())))
    })?;
    let network_id = bind_fn(lua, weak, |rt, (entity,): (LuaValue,)| {
        Ok(existing(rt, &entity)?.map_or(0, |(id, _)| i64::from(id)))
    })?;
    let entity_from_network_id = bind_fn(lua, weak, |rt, (network_id,): (LuaValue,)| {
        Ok(existing(rt, &network_id)?.map_or(0, |(id, _)| i64::from(id)))
    })?;
    let network_id_exists = bind_fn(lua, weak, |rt, (network_id,): (LuaValue,)| {
        Ok(existing(rt, &network_id)?.is_some())
    })?;
    let entity_from_bag = bind_fn(lua, weak, |rt, (bag,): (String,)| {
        let host = rt.host()?;
        Ok(entity_of_bag(&bag)
            .filter(|&id| host.entity(id).is_some())
            .map_or(0, i64::from))
    })?;
    let entity = bind_fn(lua, weak, |rt, (value,): (LuaValue,)| {
        let id = entity_id(&value).ok_or_else(|| invalid_entity("Entity"))?;
        let object = rt.lua.create_table()?;
        object.raw_set("state", state_bag(rt, &entity_bag(id))?)?;
        Ok(object)
    })?;
    let entity_script = bind_fn(lua, weak, |rt, (entity,): (LuaValue,)| {
        let Some(id) = entity_id(&entity) else {
            return Ok(None);
        };
        Ok(rt.host()?.entity_script(id))
    })?;

    let functions = [
        ("DoesEntityExist", does_exist),
        ("GetEntityCoords", coords),
        ("GetEntityHeading", heading),
        ("GetEntityRotation", rotation),
        ("GetEntityQuaternion", quaternion),
        ("GetEntityModel", model),
        ("GetEntityType", entity_type),
        ("GetEntityScript", entity_script),
        ("NetworkGetNetworkIdFromEntity", network_id),
        ("NetworkGetEntityFromNetworkId", entity_from_network_id),
        ("NetworkDoesNetworkIdExist", network_id_exists),
        ("GetEntityFromStateBagName", entity_from_bag),
        ("Entity", entity),
    ];
    for (name, function) in functions {
        globals.raw_set(name, function)?;
    }
    for (name, function) in owner_functions(lua, weak)?
        .into_iter()
        .chain(state_read_functions(lua, weak)?)
    {
        globals.raw_set(name, function)?;
    }
    Ok(())
}

/// Installs the functions only a server can use: creation, movement, deletion,
/// the owners of entities, replicated entity state and the lists of entities of each kind.
pub(crate) fn install_server(
    lua: &Lua,
    weak: &Weak<LuaRuntime>,
    globals: &Table,
) -> mlua::Result<()> {
    for (name, function) in spawn_functions(lua, weak)?
        .into_iter()
        .chain(change_functions(lua, weak)?)
        .chain(server_owner_functions(lua, weak)?)
        .chain(state_write_functions(lua, weak)?)
        .chain(list_functions(lua, weak)?)
        .chain(bucket_functions(lua, weak)?)
    {
        globals.raw_set(name, function)?;
    }
    Ok(())
}

/// The functions that create an entity.
fn spawn_functions(
    lua: &Lua,
    weak: &Weak<LuaRuntime>,
) -> mlua::Result<Vec<(&'static str, Function)>> {
    // Extra arguments, such as the network and mission flags, are ignored: the
    // tuples below only name the arguments that matter.
    let create_vehicle = bind_fn(
        lua,
        weak,
        |rt, (model, x, y, z, heading): (LuaValue, LuaValue, LuaValue, LuaValue, LuaValue)| {
            let position = coordinates(&x, &y, &z)?;
            let heading = real(&heading, "heading")?;
            spawn(
                rt,
                EntityKind::Vehicle,
                &model,
                position,
                [0.0, 0.0, heading],
            )
        },
    )?;
    let create_vehicle_setter = bind_fn(
        lua,
        weak,
        |rt,
         (model, _type, x, y, z, heading): (
            LuaValue,
            LuaValue,
            LuaValue,
            LuaValue,
            LuaValue,
            LuaValue,
        )| {
            let position = coordinates(&x, &y, &z)?;
            let heading = real(&heading, "heading")?;
            spawn(
                rt,
                EntityKind::Vehicle,
                &model,
                position,
                [0.0, 0.0, heading],
            )
        },
    )?;
    let create_ped = bind_fn(
        lua,
        weak,
        |rt,
         (_ped_type, model, x, y, z, heading): (
            LuaValue,
            LuaValue,
            LuaValue,
            LuaValue,
            LuaValue,
            LuaValue,
        )| {
            let position = coordinates(&x, &y, &z)?;
            let heading = real(&heading, "heading")?;
            spawn(rt, EntityKind::Ped, &model, position, [0.0, 0.0, heading])
        },
    )?;
    let create_object = bind_fn(
        lua,
        weak,
        |rt, (model, x, y, z): (LuaValue, LuaValue, LuaValue, LuaValue)| {
            let position = coordinates(&x, &y, &z)?;
            spawn(rt, EntityKind::Object, &model, position, [0.0; 3])
        },
    )?;
    let create_object_no_offset = create_object.clone();
    Ok(vec![
        ("CreateVehicle", create_vehicle),
        ("CreateVehicleServerSetter", create_vehicle_setter),
        ("CreatePed", create_ped),
        ("CreateObject", create_object),
        ("CreateObjectNoOffset", create_object_no_offset),
    ])
}

/// A change to the transform of an entity, as the movement functions make it.
#[derive(Debug, Clone, Copy, PartialEq)]
enum Transform {
    Position([f32; 3]),
    Rotation([f32; 3]),
}

/// Applies a transform to the entity `id`. The server moves the entity itself,
/// and the client moves an entity it owns. Both sides use the same argument
/// parsing, see [`movement_functions`].
type Apply = fn(&LuaRuntime, u32, Transform) -> mlua::Result<()>;

/// The functions that delete an entity on the server, and the functions that
/// move one, with `apply` deciding how the move is made.
fn change_functions(
    lua: &Lua,
    weak: &Weak<LuaRuntime>,
) -> mlua::Result<Vec<(&'static str, Function)>> {
    let delete_entity = bind_fn(lua, weak, |rt, (entity,): (LuaValue,)| {
        if let Some(id) = entity_id(&entity) {
            server(&*rt.host()?)?.delete_entity(id);
        }
        Ok(())
    })?;
    let mut functions = movement_functions(lua, weak, server_move)?;
    functions.push(("DeleteEntity", delete_entity));
    Ok(functions)
}

/// The functions that move an entity: `SetEntityCoords`,
/// `SetEntityCoordsNoOffset`, `SetEntityHeading` and `SetEntityRotation`.
/// Every argument is checked before the entity is looked at, so a bad number
/// fails on both sides. A heading keeps the pitch and roll of the entity.
fn movement_functions(
    lua: &Lua,
    weak: &Weak<LuaRuntime>,
    apply: Apply,
) -> mlua::Result<Vec<(&'static str, Function)>> {
    let set_coords = bind_fn(
        lua,
        weak,
        move |rt, (entity, x, y, z): (LuaValue, LuaValue, LuaValue, LuaValue)| {
            let position = coordinates(&x, &y, &z)?;
            move_entity(rt, &entity, Transform::Position(position), apply)
        },
    )?;
    let set_coords_no_offset = set_coords.clone();
    let set_heading = bind_fn(
        lua,
        weak,
        move |rt, (entity, heading): (LuaValue, LuaValue)| {
            let heading = real(&heading, "heading")?;
            match existing(rt, &entity)? {
                Some((id, info)) => {
                    let [pitch, roll, _] = info.rotation;
                    apply(rt, id, Transform::Rotation([pitch, roll, heading]))
                }
                None => Ok(()),
            }
        },
    )?;
    let set_rotation_function = bind_fn(
        lua,
        weak,
        move |rt, (entity, pitch, roll, yaw): (LuaValue, LuaValue, LuaValue, LuaValue)| {
            let rotation = [
                real(&pitch, "pitch")?,
                real(&roll, "roll")?,
                real(&yaw, "yaw")?,
            ];
            move_entity(rt, &entity, Transform::Rotation(rotation), apply)
        },
    )?;
    let set_quaternion = bind_fn(
        lua,
        weak,
        move |rt, (entity, x, y, z, w): (LuaValue, LuaValue, LuaValue, LuaValue, LuaValue)| {
            let q = [
                real(&w, "w")?,
                real(&x, "x")?,
                real(&y, "y")?,
                real(&z, "z")?,
            ];
            move_entity(rt, &entity, Transform::Rotation(quat_to_euler(q)), apply)
        },
    )?;
    Ok(vec![
        ("SetEntityCoords", set_coords),
        ("SetEntityCoordsNoOffset", set_coords_no_offset),
        ("SetEntityHeading", set_heading),
        ("SetEntityRotation", set_rotation_function),
        ("SetEntityQuaternion", set_quaternion),
    ])
}

/// The functions that read replicated entity state on both sides.
fn state_read_functions(
    lua: &Lua,
    weak: &Weak<LuaRuntime>,
) -> mlua::Result<Vec<(&'static str, Function)>> {
    let is_frozen = bind_fn(lua, weak, |rt, (entity,): (LuaValue,)| {
        Ok(existing_state(rt, &entity)?.is_some_and(|(_, state)| state.frozen))
    })?;
    let get_velocity = bind_fn(lua, weak, |rt, (entity,): (LuaValue,)| {
        let velocity = existing_state(rt, &entity)?.map_or([0.0; 3], |(_, state)| state.velocity);
        rt.lua.create_userdata(Vector::new(velocity))
    })?;
    let get_health = bind_fn(lua, weak, |rt, (entity,): (LuaValue,)| {
        Ok(existing_state(rt, &entity)?.map_or(0, |(_, state)| i64::from(state.health)))
    })?;
    let get_max_health = bind_fn(lua, weak, |rt, (entity,): (LuaValue,)| {
        Ok(existing_state(rt, &entity)?.map_or(0, |(_, state)| i64::from(state.max_health)))
    })?;
    let is_visible = bind_fn(lua, weak, |rt, (entity,): (LuaValue,)| {
        Ok(existing_state(rt, &entity)?.is_some_and(|(_, state)| state.visible))
    })?;
    let get_alpha = bind_fn(lua, weak, |rt, (entity,): (LuaValue,)| {
        Ok(existing_state(rt, &entity)?.map_or(0, |(_, state)| i64::from(state.alpha)))
    })?;
    let get_attached_to = bind_fn(lua, weak, |rt, (entity,): (LuaValue,)| {
        Ok(existing_state(rt, &entity)?
            .and_then(|(_, state)| state.attachment)
            .map_or(0, |att| i64::from(att.parent)))
    })?;
    let is_attached = bind_fn(lua, weak, |rt, (entity,): (LuaValue,)| {
        Ok(existing_state(rt, &entity)?.is_some_and(|(_, state)| state.attachment.is_some()))
    })?;
    Ok(vec![
        ("IsEntityPositionFrozen", is_frozen),
        ("GetEntityVelocity", get_velocity),
        ("GetEntityHealth", get_health),
        ("GetEntityMaxHealth", get_max_health),
        ("IsEntityVisible", is_visible),
        ("GetEntityAlpha", get_alpha),
        ("GetEntityAttachedTo", get_attached_to),
        ("IsEntityAttached", is_attached),
    ])
}

/// The functions that mutate replicated entity state on the server.
fn state_write_functions(
    lua: &Lua,
    weak: &Weak<LuaRuntime>,
) -> mlua::Result<Vec<(&'static str, Function)>> {
    let freeze = bind_fn(lua, weak, |rt, (entity, toggle): (LuaValue, bool)| {
        if let Some(id) = existing_id(rt, &entity)? {
            server(&*rt.host()?)?
                .set_entity_frozen(id, toggle)
                .map_err(lua_err)?;
        }
        Ok(())
    })?;
    let set_velocity = bind_fn(
        lua,
        weak,
        |rt, (entity, x, y, z): (LuaValue, LuaValue, LuaValue, LuaValue)| {
            let velocity = coordinates(&x, &y, &z)?;
            if let Some(id) = existing_id(rt, &entity)? {
                server(&*rt.host()?)?
                    .set_entity_velocity(id, velocity)
                    .map_err(lua_err)?;
            }
            Ok(())
        },
    )?;
    let set_health = bind_fn(lua, weak, |rt, (entity, health): (LuaValue, LuaValue)| {
        let health = non_negative_i32(&health, "health")?;
        if let Some(id) = existing_id(rt, &entity)? {
            server(&*rt.host()?)?
                .set_entity_health(id, health)
                .map_err(lua_err)?;
        }
        Ok(())
    })?;
    let set_max_health = bind_fn(lua, weak, |rt, (entity, max): (LuaValue, LuaValue)| {
        let max = non_negative_i32(&max, "max")?;
        if let Some(id) = existing_id(rt, &entity)? {
            server(&*rt.host()?)?
                .set_entity_max_health(id, max)
                .map_err(lua_err)?;
        }
        Ok(())
    })?;
    let set_visible = bind_fn(lua, weak, |rt, (entity, toggle): (LuaValue, bool)| {
        if let Some(id) = existing_id(rt, &entity)? {
            server(&*rt.host()?)?
                .set_entity_visible(id, toggle)
                .map_err(lua_err)?;
        }
        Ok(())
    })?;
    let set_alpha = bind_fn(lua, weak, |rt, (entity, alpha): (LuaValue, LuaValue)| {
        let alpha = clamped_alpha(&alpha)?;
        if let Some(id) = existing_id(rt, &entity)? {
            server(&*rt.host()?)?
                .set_entity_alpha(id, alpha)
                .map_err(lua_err)?;
        }
        Ok(())
    })?;
    let reset_alpha = bind_fn(lua, weak, |rt, (entity,): (LuaValue,)| {
        if let Some(id) = existing_id(rt, &entity)? {
            server(&*rt.host()?)?
                .set_entity_alpha(id, 255)
                .map_err(lua_err)?;
        }
        Ok(())
    })?;
    let mut functions = vec![
        ("FreezeEntityPosition", freeze),
        ("SetEntityVelocity", set_velocity),
        ("SetEntityHealth", set_health),
        ("SetEntityMaxHealth", set_max_health),
        ("SetEntityVisible", set_visible),
        ("SetEntityAlpha", set_alpha),
        ("ResetEntityAlpha", reset_alpha),
    ];
    functions.extend(attachment_write_functions(lua, weak)?);
    Ok(functions)
}

/// The server functions that attach or detach an entity.
fn attachment_write_functions(
    lua: &Lua,
    weak: &Weak<LuaRuntime>,
) -> mlua::Result<Vec<(&'static str, Function)>> {
    let attach = bind_fn(
        lua,
        weak,
        |rt,
         (entity, target, bone_index, x, y, z, rx, ry, rz): (
            LuaValue,
            LuaValue,
            LuaValue,
            LuaValue,
            LuaValue,
            LuaValue,
            LuaValue,
            LuaValue,
            LuaValue,
        )| {
            let bone_index = whole_number(&bone_index)
                .and_then(|n| i32::try_from(n).ok())
                .ok_or_else(|| {
                    mlua::Error::runtime("AttachEntityToEntity: boneIndex must be an integer")
                })?;
            let offset = coordinates(&x, &y, &z)?;
            let rotation = [real(&rx, "rx")?, real(&ry, "ry")?, real(&rz, "rz")?];
            let (Some(id), Some(target_id)) =
                (existing_id(rt, &entity)?, existing_id(rt, &target)?)
            else {
                return Ok(());
            };
            server(&*rt.host()?)?
                .attach_entity_to_entity(
                    id,
                    EntityAttachment {
                        parent: target_id,
                        bone_index,
                        offset,
                        rotation,
                    },
                )
                .map_err(lua_err)
        },
    )?;
    let detach = bind_fn(lua, weak, |rt, (entity,): (LuaValue,)| {
        if let Some(id) = existing_id(rt, &entity)? {
            server(&*rt.host()?)?.detach_entity(id).map_err(lua_err)?;
        }
        Ok(())
    })?;
    Ok(vec![
        ("AttachEntityToEntity", attach),
        ("DetachEntity", detach),
    ])
}

/// The function that tells who moves an entity. Both sides have it.
fn owner_functions(
    lua: &Lua,
    weak: &Weak<LuaRuntime>,
) -> mlua::Result<Vec<(&'static str, Function)>> {
    let owner = bind_fn(lua, weak, |rt, (entity,): (LuaValue,)| {
        // An entity without an owner, or an unknown one, reads as -1.
        Ok(existing(rt, &entity)?
            .and_then(|(_, info)| info.owner)
            .map_or(-1, i64::from))
    })?;
    Ok(vec![("NetworkGetEntityOwner", owner)])
}

/// The functions that give an entity to a player, and that choose what happens
/// to it when its creator leaves. Only the server decides owners.
fn server_owner_functions(
    lua: &Lua,
    weak: &Weak<LuaRuntime>,
) -> mlua::Result<Vec<(&'static str, Function)>> {
    let set_owner = bind_fn(lua, weak, |rt, (entity, owner): (LuaValue, LuaValue)| {
        // Player 0, or a value that is not a player, is the server.
        let owner = match whole_number(&owner) {
            Some(0) | None => 0,
            Some(player) => u32::try_from(player).map_err(|_| {
                mlua::Error::runtime("SetEntityOwner: the player must be a player id or 0")
            })?,
        };
        let Some(id) = entity_id(&entity) else {
            return Ok(false);
        };
        Ok(server(&*rt.host()?)?.set_entity_owner(id, owner))
    })?;
    let orphan_mode = bind_fn(lua, weak, |rt, (entity, mode): (LuaValue, String)| {
        let Some(id) = entity_id(&entity) else {
            return Ok(());
        };
        server(&*rt.host()?)?
            .set_entity_orphan_mode(id, &mode)
            .map_err(lua_err)
    })?;
    let first_owner = bind_fn(lua, weak, |rt, (entity,): (LuaValue,)| {
        let Some(id) = entity_id(&entity) else {
            return Ok(-1_i64);
        };
        Ok(server(&*rt.host()?)?
            .entity_first_owner(id)
            .map_or(-1, i64::from))
    })?;
    let set_cull_radius = bind_fn(lua, weak, |rt, (entity, radius): (LuaValue, LuaValue)| {
        let radius = real(&radius, "radius")?;
        let Some(id) = existing_id(rt, &entity)? else {
            return Ok(());
        };
        server(&*rt.host()?)?
            .set_entity_cull_radius(id, radius)
            .map_err(lua_err)
    })?;
    Ok(vec![
        ("SetEntityOwner", set_owner),
        ("SetEntityOrphanMode", orphan_mode),
        ("NetworkGetFirstEntityOwner", first_owner),
        ("SetEntityDistanceCullingRadius", set_cull_radius),
    ])
}

/// The routing bucket functions. The server decides the buckets, so these
/// exist only there. A setter on an unknown player or entity does nothing, and
/// a read of one gives bucket 0.
fn bucket_functions(
    lua: &Lua,
    weak: &Weak<LuaRuntime>,
) -> mlua::Result<Vec<(&'static str, Function)>> {
    let set_player_bucket = bind_fn(lua, weak, |rt, (player, bucket): (LuaValue, LuaValue)| {
        let bucket = bucket_id(&bucket, "SetPlayerRoutingBucket")?;
        if let Some(id) = player_id(&player) {
            server(&*rt.host()?)?.set_player_routing_bucket(id, bucket);
        }
        Ok(())
    })?;
    let player_bucket = bind_fn(lua, weak, |rt, (player,): (LuaValue,)| {
        let Some(id) = player_id(&player) else {
            return Ok(0_i64);
        };
        let bucket = server(&*rt.host()?)?.player_routing_bucket(id);
        Ok(i64::from(bucket.unwrap_or(0)))
    })?;
    let set_entity_bucket = bind_fn(lua, weak, |rt, (entity, bucket): (LuaValue, LuaValue)| {
        let bucket = bucket_id(&bucket, "SetEntityRoutingBucket")?;
        if let Some(id) = entity_id(&entity) {
            server(&*rt.host()?)?.set_entity_routing_bucket(id, bucket);
        }
        Ok(())
    })?;
    let entity_bucket = bind_fn(lua, weak, |rt, (entity,): (LuaValue,)| {
        let Some(id) = entity_id(&entity) else {
            return Ok(0_i64);
        };
        let bucket = server(&*rt.host()?)?.entity_routing_bucket(id);
        Ok(i64::from(bucket.unwrap_or(0)))
    })?;
    let players_in_bucket = bind_fn(lua, weak, |rt, (bucket,): (LuaValue,)| {
        let bucket = bucket_id(&bucket, "GetPlayersInRoutingBucket")?;
        Ok(server(&*rt.host()?)?.players_in_routing_bucket(bucket))
    })?;
    let set_population = bind_fn(lua, weak, |rt, (bucket, enabled): (LuaValue, bool)| {
        let bucket = bucket_id(&bucket, "SetRoutingBucketPopulationEnabled")?;
        server(&*rt.host()?)?.set_routing_bucket_population_enabled(bucket, enabled);
        Ok(())
    })?;
    let population = bind_fn(lua, weak, |rt, (bucket,): (LuaValue,)| {
        let bucket = bucket_id(&bucket, "GetRoutingBucketPopulationEnabled")?;
        Ok(server(&*rt.host()?)?.routing_bucket_population_enabled(bucket))
    })?;
    let set_lockdown = bind_fn(lua, weak, |rt, (bucket, mode): (LuaValue, String)| {
        let bucket = bucket_id(&bucket, "SetRoutingBucketEntityLockdownMode")?;
        server(&*rt.host()?)?
            .set_routing_bucket_entity_lockdown_mode(bucket, &mode)
            .map_err(lua_err)
    })?;
    let lockdown = bind_fn(lua, weak, |rt, (bucket,): (LuaValue,)| {
        let bucket = bucket_id(&bucket, "GetRoutingBucketEntityLockdownMode")?;
        Ok(server(&*rt.host()?)?.routing_bucket_entity_lockdown_mode(bucket))
    })?;
    Ok(vec![
        ("SetPlayerRoutingBucket", set_player_bucket),
        ("GetPlayerRoutingBucket", player_bucket),
        ("SetEntityRoutingBucket", set_entity_bucket),
        ("GetEntityRoutingBucket", entity_bucket),
        ("GetPlayersInRoutingBucket", players_in_bucket),
        ("SetRoutingBucketPopulationEnabled", set_population),
        ("GetRoutingBucketPopulationEnabled", population),
        ("SetRoutingBucketEntityLockdownMode", set_lockdown),
        ("GetRoutingBucketEntityLockdownMode", lockdown),
    ])
}

/// Reads a routing bucket: a whole number from 0 to 4294967295.
fn bucket_id(value: &LuaValue, function: &str) -> mlua::Result<u32> {
    whole_number(value)
        .and_then(|number| u32::try_from(number).ok())
        .ok_or_else(|| {
            mlua::Error::runtime(format!(
                "{function}: the bucket must be a whole number from 0 to 4294967295"
            ))
        })
}

/// The functions that list the entities of each kind.
fn list_functions(
    lua: &Lua,
    weak: &Weak<LuaRuntime>,
) -> mlua::Result<Vec<(&'static str, Function)>> {
    let all_vehicles = ids_function(lua, weak, EntityKind::Vehicle)?;
    let all_peds = ids_function(lua, weak, EntityKind::Ped)?;
    let all_objects = ids_function(lua, weak, EntityKind::Object)?;

    Ok(vec![
        ("GetAllVehicles", all_vehicles),
        ("GetAllPeds", all_peds),
        ("GetAllObjects", all_objects),
    ])
}

/// Installs the movement functions of a client, which move the entities this
/// client owns, and `GetGamePool`, which only a client can answer from its copy
/// of the entities.
pub(crate) fn install_client(
    lua: &Lua,
    weak: &Weak<LuaRuntime>,
    globals: &Table,
) -> mlua::Result<()> {
    for (name, function) in movement_functions(lua, weak, client_move)?
        .into_iter()
        .chain(client_create_functions(lua, weak)?)
    {
        globals.raw_set(name, function)?;
    }
    let request_control = bind_fn(lua, weak, |rt, (entity,): (LuaValue,)| {
        let Some(id) = entity_id(&entity) else {
            return Ok(false);
        };
        client(&*rt.host()?)?
            .request_entity_control(id)
            .map_err(lua_err)
    })?;
    globals.raw_set("NetworkRequestControlOfEntity", request_control)?;
    let game_pool = bind_fn(lua, weak, |rt, (name,): (String,)| {
        let kind = match name.as_str() {
            "CVehicle" => Some(EntityKind::Vehicle),
            "CPed" => Some(EntityKind::Ped),
            "CObject" => Some(EntityKind::Object),
            _ => None,
        };
        let ids = match kind {
            Some(kind) => rt.host()?.entities(Some(kind)),
            None => Vec::new(),
        };
        rt.lua.create_sequence_from(ids.into_iter().map(i64::from))
    })?;
    globals.raw_set("GetGamePool", game_pool)?;
    Ok(())
}

/// The functions that create an entity on a client. A client creates only
/// networked entities, so `isNetwork` must be `true`, and the entity is owned by
/// this client and reaches the server to be confirmed. The other arguments are
/// those of the server functions.
fn client_create_functions(
    lua: &Lua,
    weak: &Weak<LuaRuntime>,
) -> mlua::Result<Vec<(&'static str, Function)>> {
    let create_vehicle = bind_fn(
        lua,
        weak,
        |rt,
         (model, x, y, z, heading, network): (
            LuaValue,
            LuaValue,
            LuaValue,
            LuaValue,
            LuaValue,
            LuaValue,
        )| {
            let position = coordinates(&x, &y, &z)?;
            let heading = real(&heading, "heading")?;
            client_spawn(
                rt,
                EntityKind::Vehicle,
                &model,
                position,
                [0.0, 0.0, heading],
                &network,
            )
        },
    )?;
    let create_ped = bind_fn(
        lua,
        weak,
        |rt,
         (_ped_type, model, x, y, z, heading, network): (
            LuaValue,
            LuaValue,
            LuaValue,
            LuaValue,
            LuaValue,
            LuaValue,
            LuaValue,
        )| {
            let position = coordinates(&x, &y, &z)?;
            let heading = real(&heading, "heading")?;
            client_spawn(
                rt,
                EntityKind::Ped,
                &model,
                position,
                [0.0, 0.0, heading],
                &network,
            )
        },
    )?;
    let create_object = bind_fn(
        lua,
        weak,
        |rt, (model, x, y, z, network): (LuaValue, LuaValue, LuaValue, LuaValue, LuaValue)| {
            let position = coordinates(&x, &y, &z)?;
            client_spawn(rt, EntityKind::Object, &model, position, [0.0; 3], &network)
        },
    )?;
    Ok(vec![
        ("CreateVehicle", create_vehicle),
        ("CreatePed", create_ped),
        ("CreateObject", create_object),
    ])
}

/// Asks the server for an entity this client owns, and returns its id. Fails
/// unless `network` is `true`, since a client has no entities of its own.
fn client_spawn(
    rt: &LuaRuntime,
    kind: EntityKind,
    model: &LuaValue,
    position: [f32; 3],
    rotation: [f32; 3],
    network: &LuaValue,
) -> mlua::Result<i64> {
    if !matches!(network, LuaValue::Boolean(true)) {
        return Err(mlua::Error::runtime(
            "a client creates only networked entities: pass isNetwork = true",
        ));
    }
    let model = model_of(model)?;
    let host = rt.host()?;
    let client = client(&*host)?;
    let id = client
        .create_network_entity(kind, model, position, rotation)
        .map_err(lua_err)?;
    client.set_entity_script(id, rt.resource.as_ref());
    Ok(i64::from(id))
}

/// A function that returns the ids of every entity of `kind`, in ascending order.
fn ids_function(lua: &Lua, weak: &Weak<LuaRuntime>, kind: EntityKind) -> mlua::Result<Function> {
    bind_fn(lua, weak, move |rt, (): ()| {
        let ids = rt.host()?.entities(Some(kind));
        rt.lua.create_sequence_from(ids.into_iter().map(i64::from))
    })
}

/// The id and state of the entity `value` names, when it names one that exists.
fn existing(rt: &LuaRuntime, value: &LuaValue) -> mlua::Result<Option<(u32, EntityInfo)>> {
    let Some(id) = entity_id(value) else {
        return Ok(None);
    };
    Ok(rt.host()?.entity(id).map(|info| (id, info)))
}

/// The id of the entity `value` names, when it names one that exists.
fn existing_id(rt: &LuaRuntime, value: &LuaValue) -> mlua::Result<Option<u32>> {
    let Some(id) = entity_id(value) else {
        return Ok(None);
    };
    Ok(rt.host()?.entity(id).map(|_| id))
}

/// The id and replicated state of the entity `value` names, when it names one
/// that exists.
fn existing_state(rt: &LuaRuntime, value: &LuaValue) -> mlua::Result<Option<(u32, EntityState)>> {
    let Some(id) = entity_id(value) else {
        return Ok(None);
    };
    Ok(rt.host()?.entity_state(id).map(|state| (id, state)))
}

/// Creates an entity of `kind` with the given model, position and rotation, and
/// returns its id.
fn spawn(
    rt: &LuaRuntime,
    kind: EntityKind,
    model: &LuaValue,
    position: [f32; 3],
    rotation: [f32; 3],
) -> mlua::Result<i64> {
    let info = EntityInfo {
        kind,
        model: model_of(model)?,
        position,
        rotation,
        owner: None,
    };
    let host = rt.host()?;
    let srv = server(&*host)?;
    let id = srv.create_entity(info).map_err(lua_err)?;
    srv.set_entity_script(id, rt.resource.as_ref());
    Ok(i64::from(id))
}

/// Applies `transform` to the entity `entity` names. A value that names no
/// entity does nothing.
fn move_entity(
    rt: &LuaRuntime,
    entity: &LuaValue,
    transform: Transform,
    apply: Apply,
) -> mlua::Result<()> {
    match entity_id(entity) {
        Some(id) => apply(rt, id, transform),
        None => Ok(()),
    }
}

/// The server's move: it sets the transform, which reaches every client, and
/// the owner of the entity takes it too. An unknown entity does nothing.
fn server_move(rt: &LuaRuntime, id: u32, transform: Transform) -> mlua::Result<()> {
    let host = rt.host()?;
    if host.entity(id).is_none() {
        return Ok(());
    }
    let server = server(&*host)?;
    match transform {
        Transform::Position(position) => server.set_entity_position(id, position),
        Transform::Rotation(rotation) => server.set_entity_rotation(id, rotation),
    }
    .map_err(lua_err)
}

/// The client's move of an entity it owns. An entity this client does not own,
/// or does not know, is left alone, as the server leaves unknown entities alone.
fn client_move(rt: &LuaRuntime, id: u32, transform: Transform) -> mlua::Result<()> {
    let host = rt.host()?;
    let owner_host = client(&*host)?;
    let (position, rotation) = match transform {
        Transform::Position(position) => (Some(position), None),
        Transform::Rotation(rotation) => (None, Some(rotation)),
    };
    match owner_host.move_owned_entity(id, position, rotation) {
        Ok(()) => Ok(()),
        Err(err) => {
            // The host refuses only what it does not own, or cannot find, and
            // the arguments were checked already, so anything else is an error.
            let owned =
                host.entity(id).and_then(|info| info.owner) == Some(owner_host.local_player());
            if owned { Err(lua_err(err)) } else { Ok(()) }
        }
    }
}

/// The model hash `value` names. A string is hashed, and a whole number is the
/// hash itself, so the value of `GetHashKey` works as a model.
fn model_of(value: &LuaValue) -> mlua::Result<u32> {
    match value {
        LuaValue::String(text) => Ok(joaat(&text.as_bytes())),
        other => whole_number(other)
            .map(model_bits)
            .ok_or_else(|| mlua::Error::runtime("the model must be a number or a string")),
    }
}

/// The 32 bits of a model hash. A negative number keeps its bits, so the signed
/// value of `GetHashKey` names the same model.
#[expect(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    reason = "a model hash is 32 bits, and a script may pass it as a signed integer"
)]
fn model_bits(number: i64) -> u32 {
    number as u32
}

/// A model hash as `GetHashKey` returns it: the 32 bits read as a signed integer.
#[expect(
    clippy::cast_possible_wrap,
    reason = "GetHashKey returns the hash as a signed 32-bit integer"
)]
fn signed_hash(model: u32) -> i64 {
    i64::from(model as i32)
}

/// The entity id a value names: a whole number that fits 32 bits.
fn entity_id(value: &LuaValue) -> Option<u32> {
    whole_number(value).and_then(|number| u32::try_from(number).ok())
}

/// The number `value` holds, as a component. Fails for a value that is not a
/// number, or for one that is not finite.
fn real(value: &LuaValue, what: &str) -> mlua::Result<f32> {
    match number_of(value) {
        Some(number) if number.is_finite() => Ok(number),
        Some(_) => Err(mlua::Error::runtime(format!(
            "{what} must be a finite number"
        ))),
        None => Err(mlua::Error::runtime(format!("{what} must be a number"))),
    }
}

/// The finite position that `x`, `y` and `z` hold.
fn coordinates(x: &LuaValue, y: &LuaValue, z: &LuaValue) -> mlua::Result<[f32; 3]> {
    Ok([real(x, "x")?, real(y, "y")?, real(z, "z")?])
}

/// Reads a finite number and clamps it into `0..=i32::MAX`.
#[expect(
    clippy::cast_possible_truncation,
    reason = "the value is finite and clamped to 0..=i32::MAX first"
)]
fn non_negative_i32(value: &LuaValue, what: &str) -> mlua::Result<i32> {
    match value {
        LuaValue::Integer(int) => {
            Ok(i32::try_from((*int).clamp(0, i64::from(i32::MAX))).unwrap_or(i32::MAX))
        }
        LuaValue::Number(float) if float.is_finite() => {
            Ok(float.round().clamp(0.0, f64::from(i32::MAX)) as i32)
        }
        LuaValue::Number(_) => Err(mlua::Error::runtime(format!(
            "{what} must be a finite number"
        ))),
        _ => Err(mlua::Error::runtime(format!("{what} must be a number"))),
    }
}

/// Reads a finite number and clamps it into `0..=255`.
#[expect(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    reason = "the value is finite and clamped to 0..=255 first"
)]
fn clamped_alpha(value: &LuaValue) -> mlua::Result<u8> {
    match value {
        LuaValue::Integer(int) => Ok(u8::try_from((*int).clamp(0, 255)).unwrap_or(255)),
        LuaValue::Number(float) if float.is_finite() => Ok(float.round().clamp(0.0, 255.0) as u8),
        LuaValue::Number(_) => Err(mlua::Error::runtime("alpha must be a finite number")),
        _ => Err(mlua::Error::runtime("alpha must be a number")),
    }
}

fn invalid_entity(function: &str) -> mlua::Error {
    mlua::Error::runtime(format!("{function}: expected an entity id"))
}
