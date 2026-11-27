//! Entity functions: the natives that read entities on both sides, the ones
//! that create, move and delete them on the server, `GetGamePool` on the client,
//! and `Entity(id)` for an entity's state bag.
//!
//! Every read asks the host, so an unknown entity reads as the neutral value of
//! its type: a zero vector, zero, or `false`. A setter on an unknown entity does
//! nothing. A number that is not finite raises an error.

use std::rc::Weak;

use mlua::{Function, Lua, Table, Value as LuaValue};
use sixm_core::{EntityInfo, EntityKind, entity_bag, entity_of_bag, joaat};

use crate::players::{server, whole_number};
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

    let functions = [
        ("DoesEntityExist", does_exist),
        ("GetEntityCoords", coords),
        ("GetEntityHeading", heading),
        ("GetEntityRotation", rotation),
        ("GetEntityModel", model),
        ("GetEntityType", entity_type),
        ("NetworkGetNetworkIdFromEntity", network_id),
        ("NetworkGetEntityFromNetworkId", entity_from_network_id),
        ("NetworkDoesNetworkIdExist", network_id_exists),
        ("GetEntityFromStateBagName", entity_from_bag),
        ("Entity", entity),
    ];
    for (name, function) in functions {
        globals.raw_set(name, function)?;
    }
    Ok(())
}

/// Installs the functions only a server can use: creation, movement, deletion
/// and the lists of entities of each kind.
pub(crate) fn install_server(
    lua: &Lua,
    weak: &Weak<LuaRuntime>,
    globals: &Table,
) -> mlua::Result<()> {
    for (name, function) in spawn_functions(lua, weak)?
        .into_iter()
        .chain(change_functions(lua, weak)?)
        .chain(list_functions(lua, weak)?)
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

/// The functions that move or delete an entity.
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
    let set_coords = bind_fn(
        lua,
        weak,
        |rt, (entity, x, y, z): (LuaValue, LuaValue, LuaValue, LuaValue)| {
            let position = coordinates(&x, &y, &z)?;
            move_entity(rt, &entity, position)
        },
    )?;
    let set_coords_no_offset = set_coords.clone();
    let set_heading = bind_fn(lua, weak, |rt, (entity, heading): (LuaValue, LuaValue)| {
        let heading = real(&heading, "heading")?;
        match existing(rt, &entity)? {
            Some((id, info)) => {
                let [pitch, roll, _] = info.rotation;
                set_rotation(rt, id, [pitch, roll, heading])
            }
            None => Ok(()),
        }
    })?;
    let set_rotation_function = bind_fn(
        lua,
        weak,
        |rt, (entity, pitch, roll, yaw): (LuaValue, LuaValue, LuaValue, LuaValue)| {
            let rotation = [
                real(&pitch, "pitch")?,
                real(&roll, "roll")?,
                real(&yaw, "yaw")?,
            ];
            match existing(rt, &entity)? {
                Some((id, _)) => set_rotation(rt, id, rotation),
                None => Ok(()),
            }
        },
    )?;
    Ok(vec![
        ("DeleteEntity", delete_entity),
        ("SetEntityCoords", set_coords),
        ("SetEntityCoordsNoOffset", set_coords_no_offset),
        ("SetEntityHeading", set_heading),
        ("SetEntityRotation", set_rotation_function),
    ])
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

/// Installs `GetGamePool`, which only a client can answer from its copy of the
/// entities.
pub(crate) fn install_client(
    lua: &Lua,
    weak: &Weak<LuaRuntime>,
    globals: &Table,
) -> mlua::Result<()> {
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
    };
    let host = rt.host()?;
    let id = server(&*host)?.create_entity(info).map_err(lua_err)?;
    Ok(i64::from(id))
}

/// Moves the entity `entity` names, when it exists.
fn move_entity(rt: &LuaRuntime, entity: &LuaValue, position: [f32; 3]) -> mlua::Result<()> {
    let Some((id, _)) = existing(rt, entity)? else {
        return Ok(());
    };
    let host = rt.host()?;
    server(&*host)?
        .set_entity_position(id, position)
        .map_err(lua_err)
}

/// Rotates the entity `id`.
fn set_rotation(rt: &LuaRuntime, id: u32, rotation: [f32; 3]) -> mlua::Result<()> {
    let host = rt.host()?;
    server(&*host)?
        .set_entity_rotation(id, rotation)
        .map_err(lua_err)
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

fn invalid_entity(function: &str) -> mlua::Error {
    mlua::Error::runtime(format!("{function}: expected an entity id"))
}
