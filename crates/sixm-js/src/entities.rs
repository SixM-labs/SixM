//! Entity natives on both sides, server entity creation/deletion/ownership/routing
//! buckets, and client entity creation/control/`GetGamePool`.

use std::rc::Weak;

use rquickjs::function::{Flat, Opt};
use rquickjs::{Ctx, Exception, Function, Object, Value as JsValue};
use sixm_core::{EntityInfo, EntityKind, entity_bag, entity_of_bag, joaat};

use crate::client::client;
use crate::players::{player_id, server, whole_number};
use crate::runtime::JsRuntime;
use crate::state::state_bag;
use crate::vector::{make_vector, number_of};

pub(crate) fn install(ctx: &Ctx<'_>, weak: &Weak<JsRuntime>) -> rquickjs::Result<()> {
    install_entity_queries(ctx, weak)?;
    install_entity_meta(ctx, weak)?;
    Ok(())
}

fn install_entity_queries<'js>(ctx: &Ctx<'js>, weak: &Weak<JsRuntime>) -> rquickjs::Result<()> {
    let globals = ctx.globals();
    let does_exist = {
        let weak = weak.clone();
        Function::new(
            ctx.clone(),
            move |ctx: Ctx<'js>, entity: JsValue<'js>| -> rquickjs::Result<bool> {
                let rt = JsRuntime::upgrade(&ctx, &weak)?;
                Ok(existing(&rt, &ctx, &entity)?.is_some())
            },
        )?
    };
    let coords = {
        let weak = weak.clone();
        Function::new(
            ctx.clone(),
            move |ctx: Ctx<'js>, entity: JsValue<'js>| -> rquickjs::Result<JsValue<'js>> {
                let rt = JsRuntime::upgrade(&ctx, &weak)?;
                let pos =
                    existing(&rt, &ctx, &entity)?.map_or([0.0; 3], |(_, info)| info.position);
                make_vector(&ctx, pos)
            },
        )?
    };
    let heading = {
        let weak = weak.clone();
        Function::new(
            ctx.clone(),
            move |ctx: Ctx<'js>, entity: JsValue<'js>| -> rquickjs::Result<f64> {
                let rt = JsRuntime::upgrade(&ctx, &weak)?;
                Ok(existing(&rt, &ctx, &entity)?
                    .map_or(0.0, |(_, info)| f64::from(info.rotation[2])))
            },
        )?
    };
    let rotation = {
        let weak = weak.clone();
        Function::new(
            ctx.clone(),
            move |ctx: Ctx<'js>,
                  entity: JsValue<'js>,
                  _order: Opt<JsValue<'js>>|
                  -> rquickjs::Result<JsValue<'js>> {
                let rt = JsRuntime::upgrade(&ctx, &weak)?;
                let rot =
                    existing(&rt, &ctx, &entity)?.map_or([0.0; 3], |(_, info)| info.rotation);
                make_vector(&ctx, rot)
            },
        )?
    };
    let model = {
        let weak = weak.clone();
        Function::new(
            ctx.clone(),
            move |ctx: Ctx<'js>, entity: JsValue<'js>| -> rquickjs::Result<i64> {
                let rt = JsRuntime::upgrade(&ctx, &weak)?;
                Ok(existing(&rt, &ctx, &entity)?.map_or(0, |(_, info)| signed_hash(info.model)))
            },
        )?
    };
    let entity_type = {
        let weak = weak.clone();
        Function::new(
            ctx.clone(),
            move |ctx: Ctx<'js>, entity: JsValue<'js>| -> rquickjs::Result<i64> {
                let rt = JsRuntime::upgrade(&ctx, &weak)?;
                Ok(existing(&rt, &ctx, &entity)?
                    .map_or(0, |(_, info)| i64::from(info.kind.code())))
            },
        )?
    };
    for (name, func) in [
        ("DoesEntityExist", does_exist),
        ("GetEntityCoords", coords),
        ("GetEntityHeading", heading),
        ("GetEntityRotation", rotation),
        ("GetEntityModel", model),
        ("GetEntityType", entity_type),
    ] {
        globals.set(name, func)?;
    }
    Ok(())
}

fn install_entity_meta<'js>(ctx: &Ctx<'js>, weak: &Weak<JsRuntime>) -> rquickjs::Result<()> {
    let globals = ctx.globals();
    let network_id = {
        let weak = weak.clone();
        Function::new(
            ctx.clone(),
            move |ctx: Ctx<'js>, entity: JsValue<'js>| -> rquickjs::Result<i64> {
                let rt = JsRuntime::upgrade(&ctx, &weak)?;
                Ok(existing(&rt, &ctx, &entity)?.map_or(0, |(id, _)| i64::from(id)))
            },
        )?
    };
    let entity_from_net_id = {
        let weak = weak.clone();
        Function::new(
            ctx.clone(),
            move |ctx: Ctx<'js>, net_id: JsValue<'js>| -> rquickjs::Result<i64> {
                let rt = JsRuntime::upgrade(&ctx, &weak)?;
                Ok(existing(&rt, &ctx, &net_id)?.map_or(0, |(id, _)| i64::from(id)))
            },
        )?
    };
    let net_id_exists = {
        let weak = weak.clone();
        Function::new(
            ctx.clone(),
            move |ctx: Ctx<'js>, net_id: JsValue<'js>| -> rquickjs::Result<bool> {
                let rt = JsRuntime::upgrade(&ctx, &weak)?;
                Ok(existing(&rt, &ctx, &net_id)?.is_some())
            },
        )?
    };
    let entity_from_bag = {
        let weak = weak.clone();
        Function::new(
            ctx.clone(),
            move |ctx: Ctx<'js>, bag: String| -> rquickjs::Result<i64> {
                let rt = JsRuntime::upgrade(&ctx, &weak)?;
                let host = rt.host(&ctx)?;
                Ok(entity_of_bag(&bag)
                    .filter(|&id| host.entity(id).is_some())
                    .map_or(0, i64::from))
            },
        )?
    };
    let entity_obj = {
        let weak = weak.clone();
        Function::new(
            ctx.clone(),
            move |ctx: Ctx<'js>, value: JsValue<'js>| -> rquickjs::Result<Object<'js>> {
                let _ = JsRuntime::upgrade(&ctx, &weak)?;
                let id = entity_id(&value).ok_or_else(|| invalid_entity(&ctx, "Entity"))?;
                let obj = Object::new(ctx.clone())?;
                obj.set("state", state_bag(&ctx, &weak, &entity_bag(id))?)?;
                Ok(obj)
            },
        )?
    };
    let owner = {
        let weak = weak.clone();
        Function::new(
            ctx.clone(),
            move |ctx: Ctx<'js>, entity: JsValue<'js>| -> rquickjs::Result<i64> {
                let rt = JsRuntime::upgrade(&ctx, &weak)?;
                Ok(existing(&rt, &ctx, &entity)?
                    .and_then(|(_, info)| info.owner)
                    .map_or(-1, i64::from))
            },
        )?
    };
    for (name, func) in [
        ("NetworkGetNetworkIdFromEntity", network_id),
        ("NetworkGetEntityFromNetworkId", entity_from_net_id),
        ("NetworkDoesNetworkIdExist", net_id_exists),
        ("GetEntityFromStateBagName", entity_from_bag),
        ("Entity", entity_obj),
        ("NetworkGetEntityOwner", owner),
    ] {
        globals.set(name, func)?;
    }
    Ok(())
}

pub(crate) fn install_server(ctx: &Ctx<'_>, weak: &Weak<JsRuntime>) -> rquickjs::Result<()> {
    let globals = ctx.globals();
    for (name, func) in spawn_functions(ctx, weak)?
        .into_iter()
        .chain(change_functions(ctx, weak)?)
        .chain(server_owner_functions(ctx, weak)?)
        .chain(list_functions(ctx, weak)?)
        .chain(bucket_functions(ctx, weak)?)
    {
        globals.set(name, func)?;
    }
    Ok(())
}

pub(crate) fn install_client<'js>(ctx: &Ctx<'js>, weak: &Weak<JsRuntime>) -> rquickjs::Result<()> {
    let globals = ctx.globals();
    for (name, func) in movement_functions(ctx, weak, client_move)?
        .into_iter()
        .chain(client_create_functions(ctx, weak)?)
    {
        globals.set(name, func)?;
    }

    {
        let weak = weak.clone();
        globals.set(
            "NetworkRequestControlOfEntity",
            Function::new(
                ctx.clone(),
                move |ctx: Ctx<'js>, entity: JsValue<'js>| -> rquickjs::Result<bool> {
                    let rt = JsRuntime::upgrade(&ctx, &weak)?;
                    let Some(id) = entity_id(&entity) else {
                        return Ok(false);
                    };
                    client(&ctx, &*rt.host(&ctx)?)?
                        .request_entity_control(id)
                        .map_err(|err| Exception::throw_message(&ctx, &err.to_string()))
                },
            )?,
        )?;
    }

    {
        let weak = weak.clone();
        globals.set(
            "GetGamePool",
            Function::new(
                ctx.clone(),
                move |ctx: Ctx<'js>, name: String| -> rquickjs::Result<Vec<i64>> {
                    let rt = JsRuntime::upgrade(&ctx, &weak)?;
                    let kind = match name.as_str() {
                        "CVehicle" => Some(EntityKind::Vehicle),
                        "CPed" => Some(EntityKind::Ped),
                        "CObject" => Some(EntityKind::Object),
                        _ => None,
                    };
                    let ids = match kind {
                        Some(kind) => rt.host(&ctx)?.entities(Some(kind)),
                        None => Vec::new(),
                    };
                    Ok(ids.into_iter().map(i64::from).collect())
                },
            )?,
        )?;
    }

    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum Transform {
    Position([f32; 3]),
    Rotation([f32; 3]),
}

type Apply = fn(&JsRuntime, &Ctx<'_>, u32, Transform) -> rquickjs::Result<()>;

fn spawn_functions<'js>(
    ctx: &Ctx<'js>,
    weak: &Weak<JsRuntime>,
) -> rquickjs::Result<Vec<(&'static str, Function<'js>)>> {
    let create_vehicle = {
        let weak = weak.clone();
        Function::new(
            ctx.clone(),
            move |ctx: Ctx<'js>,
                  model: JsValue<'js>,
                  x: JsValue<'js>,
                  y: JsValue<'js>,
                  z: JsValue<'js>,
                  heading: JsValue<'js>|
                  -> rquickjs::Result<i64> {
                let rt = JsRuntime::upgrade(&ctx, &weak)?;
                let pos = coordinates(&ctx, &x, &y, &z)?;
                let heading = real(&ctx, &heading, "heading")?;
                spawn(&rt, &ctx, EntityKind::Vehicle, &model, pos, [0.0, 0.0, heading])
            },
        )?
    };
    let create_vehicle_setter = {
        let weak = weak.clone();
        Function::new(
            ctx.clone(),
            move |ctx: Ctx<'js>,
                  model: JsValue<'js>,
                  _kind: JsValue<'js>,
                  x: JsValue<'js>,
                  y: JsValue<'js>,
                  z: JsValue<'js>,
                  heading: JsValue<'js>|
                  -> rquickjs::Result<i64> {
                let rt = JsRuntime::upgrade(&ctx, &weak)?;
                let pos = coordinates(&ctx, &x, &y, &z)?;
                let heading = real(&ctx, &heading, "heading")?;
                spawn(&rt, &ctx, EntityKind::Vehicle, &model, pos, [0.0, 0.0, heading])
            },
        )?
    };
    let create_ped = {
        let weak = weak.clone();
        Function::new(
            ctx.clone(),
            move |ctx: Ctx<'js>,
                  _ped_type: JsValue<'js>,
                  model: JsValue<'js>,
                  x: JsValue<'js>,
                  y: JsValue<'js>,
                  z: JsValue<'js>,
                  heading: JsValue<'js>|
                  -> rquickjs::Result<i64> {
                let rt = JsRuntime::upgrade(&ctx, &weak)?;
                let pos = coordinates(&ctx, &x, &y, &z)?;
                let heading = real(&ctx, &heading, "heading")?;
                spawn(&rt, &ctx, EntityKind::Ped, &model, pos, [0.0, 0.0, heading])
            },
        )?
    };
    let create_object = {
        let weak = weak.clone();
        Function::new(
            ctx.clone(),
            move |ctx: Ctx<'js>,
                  model: JsValue<'js>,
                  x: JsValue<'js>,
                  y: JsValue<'js>,
                  z: JsValue<'js>|
                  -> rquickjs::Result<i64> {
                let rt = JsRuntime::upgrade(&ctx, &weak)?;
                let pos = coordinates(&ctx, &x, &y, &z)?;
                spawn(&rt, &ctx, EntityKind::Object, &model, pos, [0.0; 3])
            },
        )?
    };
    let create_object_no_offset = create_object.clone();
    Ok(vec![
        ("CreateVehicle", create_vehicle),
        ("CreateVehicleServerSetter", create_vehicle_setter),
        ("CreatePed", create_ped),
        ("CreateObject", create_object),
        ("CreateObjectNoOffset", create_object_no_offset),
    ])
}

fn client_create_functions<'js>(
    ctx: &Ctx<'js>,
    weak: &Weak<JsRuntime>,
) -> rquickjs::Result<Vec<(&'static str, Function<'js>)>> {
    let create_vehicle = {
        let weak = weak.clone();
        Function::new(
            ctx.clone(),
            move |ctx: Ctx<'js>,
                  model: JsValue<'js>,
                  x: JsValue<'js>,
                  y: JsValue<'js>,
                  z: JsValue<'js>,
                  heading: JsValue<'js>,
                  network: Opt<JsValue<'js>>|
                  -> rquickjs::Result<i64> {
                let rt = JsRuntime::upgrade(&ctx, &weak)?;
                let pos = coordinates(&ctx, &x, &y, &z)?;
                let heading = real(&ctx, &heading, "heading")?;
                client_spawn(
                    &rt,
                    &ctx,
                    EntityKind::Vehicle,
                    &model,
                    pos,
                    [0.0, 0.0, heading],
                    network.0.as_ref(),
                )
            },
        )?
    };
    let create_ped = {
        let weak = weak.clone();
        Function::new(
            ctx.clone(),
            move |ctx: Ctx<'js>,
                  _ped_type: JsValue<'js>,
                  model: JsValue<'js>,
                  x: JsValue<'js>,
                  y: JsValue<'js>,
                  z: JsValue<'js>,
                  Flat((heading, network)): Flat<(JsValue<'js>, Opt<JsValue<'js>>)>|
                  -> rquickjs::Result<i64> {
                let rt = JsRuntime::upgrade(&ctx, &weak)?;
                let pos = coordinates(&ctx, &x, &y, &z)?;
                let heading = real(&ctx, &heading, "heading")?;
                client_spawn(
                    &rt,
                    &ctx,
                    EntityKind::Ped,
                    &model,
                    pos,
                    [0.0, 0.0, heading],
                    network.0.as_ref(),
                )
            },
        )?
    };
    let create_object = {
        let weak = weak.clone();
        Function::new(
            ctx.clone(),
            move |ctx: Ctx<'js>,
                  model: JsValue<'js>,
                  x: JsValue<'js>,
                  y: JsValue<'js>,
                  z: JsValue<'js>,
                  network: Opt<JsValue<'js>>|
                  -> rquickjs::Result<i64> {
                let rt = JsRuntime::upgrade(&ctx, &weak)?;
                let pos = coordinates(&ctx, &x, &y, &z)?;
                client_spawn(
                    &rt,
                    &ctx,
                    EntityKind::Object,
                    &model,
                    pos,
                    [0.0; 3],
                    network.0.as_ref(),
                )
            },
        )?
    };
    Ok(vec![
        ("CreateVehicle", create_vehicle),
        ("CreatePed", create_ped),
        ("CreateObject", create_object),
    ])
}

fn change_functions<'js>(
    ctx: &Ctx<'js>,
    weak: &Weak<JsRuntime>,
) -> rquickjs::Result<Vec<(&'static str, Function<'js>)>> {
    let delete_entity = {
        let weak = weak.clone();
        Function::new(
            ctx.clone(),
            move |ctx: Ctx<'js>, entity: JsValue<'js>| -> rquickjs::Result<()> {
                let rt = JsRuntime::upgrade(&ctx, &weak)?;
                if let Some(id) = entity_id(&entity) {
                    server(&ctx, &*rt.host(&ctx)?)?.delete_entity(id);
                }
                Ok(())
            },
        )?
    };
    let mut functions = movement_functions(ctx, weak, server_move)?;
    functions.push(("DeleteEntity", delete_entity));
    Ok(functions)
}

fn movement_functions<'js>(
    ctx: &Ctx<'js>,
    weak: &Weak<JsRuntime>,
    apply: Apply,
) -> rquickjs::Result<Vec<(&'static str, Function<'js>)>> {
    let set_coords = {
        let weak = weak.clone();
        Function::new(
            ctx.clone(),
            move |ctx: Ctx<'js>,
                  entity: JsValue<'js>,
                  x: JsValue<'js>,
                  y: JsValue<'js>,
                  z: JsValue<'js>|
                  -> rquickjs::Result<()> {
                let rt = JsRuntime::upgrade(&ctx, &weak)?;
                let pos = coordinates(&ctx, &x, &y, &z)?;
                move_entity(&rt, &ctx, &entity, Transform::Position(pos), apply)
            },
        )?
    };
    let set_coords_no_offset = set_coords.clone();
    let set_heading = {
        let weak = weak.clone();
        Function::new(
            ctx.clone(),
            move |ctx: Ctx<'js>, entity: JsValue<'js>, heading: JsValue<'js>| -> rquickjs::Result<()> {
                let rt = JsRuntime::upgrade(&ctx, &weak)?;
                let heading = real(&ctx, &heading, "heading")?;
                match existing(&rt, &ctx, &entity)? {
                    Some((id, info)) => {
                        let [pitch, roll, _] = info.rotation;
                        apply(&rt, &ctx, id, Transform::Rotation([pitch, roll, heading]))
                    }
                    None => Ok(()),
                }
            },
        )?
    };
    let set_rotation = {
        let weak = weak.clone();
        Function::new(
            ctx.clone(),
            move |ctx: Ctx<'js>,
                  entity: JsValue<'js>,
                  pitch: JsValue<'js>,
                  roll: JsValue<'js>,
                  yaw: JsValue<'js>|
                  -> rquickjs::Result<()> {
                let rt = JsRuntime::upgrade(&ctx, &weak)?;
                let rot = [
                    real(&ctx, &pitch, "pitch")?,
                    real(&ctx, &roll, "roll")?,
                    real(&ctx, &yaw, "yaw")?,
                ];
                move_entity(&rt, &ctx, &entity, Transform::Rotation(rot), apply)
            },
        )?
    };
    Ok(vec![
        ("SetEntityCoords", set_coords),
        ("SetEntityCoordsNoOffset", set_coords_no_offset),
        ("SetEntityHeading", set_heading),
        ("SetEntityRotation", set_rotation),
    ])
}

fn server_owner_functions<'js>(
    ctx: &Ctx<'js>,
    weak: &Weak<JsRuntime>,
) -> rquickjs::Result<Vec<(&'static str, Function<'js>)>> {
    let set_owner = {
        let weak = weak.clone();
        Function::new(
            ctx.clone(),
            move |ctx: Ctx<'js>, entity: JsValue<'js>, owner: JsValue<'js>| -> rquickjs::Result<bool> {
                let rt = JsRuntime::upgrade(&ctx, &weak)?;
                let owner = match whole_number(&owner) {
                    Some(0) | None => 0,
                    Some(player) => u32::try_from(player).map_err(|_| {
                        Exception::throw_type(
                            &ctx,
                            "SetEntityOwner: the player must be a player id or 0",
                        )
                    })?,
                };
                let Some(id) = entity_id(&entity) else {
                    return Ok(false);
                };
                Ok(server(&ctx, &*rt.host(&ctx)?)?.set_entity_owner(id, owner))
            },
        )?
    };
    let orphan_mode = {
        let weak = weak.clone();
        Function::new(
            ctx.clone(),
            move |ctx: Ctx<'js>, entity: JsValue<'js>, mode: String| -> rquickjs::Result<()> {
                let rt = JsRuntime::upgrade(&ctx, &weak)?;
                let Some(id) = entity_id(&entity) else {
                    return Ok(());
                };
                server(&ctx, &*rt.host(&ctx)?)?
                    .set_entity_orphan_mode(id, &mode)
                    .map_err(|err| Exception::throw_message(&ctx, &err.to_string()))
            },
        )?
    };
    Ok(vec![
        ("SetEntityOwner", set_owner),
        ("SetEntityOrphanMode", orphan_mode),
    ])
}

fn bucket_functions<'js>(
    ctx: &Ctx<'js>,
    weak: &Weak<JsRuntime>,
) -> rquickjs::Result<Vec<(&'static str, Function<'js>)>> {
    let mut funcs = bucket_member_functions(ctx, weak)?;
    funcs.extend(bucket_rule_functions(ctx, weak)?);
    Ok(funcs)
}

fn bucket_member_functions<'js>(
    ctx: &Ctx<'js>,
    weak: &Weak<JsRuntime>,
) -> rquickjs::Result<Vec<(&'static str, Function<'js>)>> {
    let set_player_bucket = {
        let weak = weak.clone();
        Function::new(
            ctx.clone(),
            move |ctx: Ctx<'js>, player: JsValue<'js>, bucket: JsValue<'js>| -> rquickjs::Result<()> {
                let rt = JsRuntime::upgrade(&ctx, &weak)?;
                let bucket = bucket_id(&ctx, &bucket, "SetPlayerRoutingBucket")?;
                if let Some(id) = player_id(&player) {
                    server(&ctx, &*rt.host(&ctx)?)?.set_player_routing_bucket(id, bucket);
                }
                Ok(())
            },
        )?
    };
    let player_bucket = {
        let weak = weak.clone();
        Function::new(
            ctx.clone(),
            move |ctx: Ctx<'js>, player: JsValue<'js>| -> rquickjs::Result<i64> {
                let rt = JsRuntime::upgrade(&ctx, &weak)?;
                let Some(id) = player_id(&player) else {
                    return Ok(0_i64);
                };
                let bucket = server(&ctx, &*rt.host(&ctx)?)?.player_routing_bucket(id);
                Ok(i64::from(bucket.unwrap_or(0)))
            },
        )?
    };
    let set_entity_bucket = {
        let weak = weak.clone();
        Function::new(
            ctx.clone(),
            move |ctx: Ctx<'js>, entity: JsValue<'js>, bucket: JsValue<'js>| -> rquickjs::Result<()> {
                let rt = JsRuntime::upgrade(&ctx, &weak)?;
                let bucket = bucket_id(&ctx, &bucket, "SetEntityRoutingBucket")?;
                if let Some(id) = entity_id(&entity) {
                    server(&ctx, &*rt.host(&ctx)?)?.set_entity_routing_bucket(id, bucket);
                }
                Ok(())
            },
        )?
    };
    let entity_bucket = {
        let weak = weak.clone();
        Function::new(
            ctx.clone(),
            move |ctx: Ctx<'js>, entity: JsValue<'js>| -> rquickjs::Result<i64> {
                let rt = JsRuntime::upgrade(&ctx, &weak)?;
                let Some(id) = entity_id(&entity) else {
                    return Ok(0_i64);
                };
                let bucket = server(&ctx, &*rt.host(&ctx)?)?.entity_routing_bucket(id);
                Ok(i64::from(bucket.unwrap_or(0)))
            },
        )?
    };
    let players_in_bucket = {
        let weak = weak.clone();
        Function::new(
            ctx.clone(),
            move |ctx: Ctx<'js>, bucket: JsValue<'js>| -> rquickjs::Result<Vec<i64>> {
                let rt = JsRuntime::upgrade(&ctx, &weak)?;
                let bucket = bucket_id(&ctx, &bucket, "GetPlayersInRoutingBucket")?;
                let ids = server(&ctx, &*rt.host(&ctx)?)?.players_in_routing_bucket(bucket);
                Ok(ids.into_iter().map(i64::from).collect())
            },
        )?
    };
    Ok(vec![
        ("SetPlayerRoutingBucket", set_player_bucket),
        ("GetPlayerRoutingBucket", player_bucket),
        ("SetEntityRoutingBucket", set_entity_bucket),
        ("GetEntityRoutingBucket", entity_bucket),
        ("GetPlayersInRoutingBucket", players_in_bucket),
    ])
}

fn bucket_rule_functions<'js>(
    ctx: &Ctx<'js>,
    weak: &Weak<JsRuntime>,
) -> rquickjs::Result<Vec<(&'static str, Function<'js>)>> {
    let set_population = {
        let weak = weak.clone();
        Function::new(
            ctx.clone(),
            move |ctx: Ctx<'js>, bucket: JsValue<'js>, enabled: bool| -> rquickjs::Result<()> {
                let rt = JsRuntime::upgrade(&ctx, &weak)?;
                let bucket = bucket_id(&ctx, &bucket, "SetRoutingBucketPopulationEnabled")?;
                server(&ctx, &*rt.host(&ctx)?)?
                    .set_routing_bucket_population_enabled(bucket, enabled);
                Ok(())
            },
        )?
    };
    let population = {
        let weak = weak.clone();
        Function::new(
            ctx.clone(),
            move |ctx: Ctx<'js>, bucket: JsValue<'js>| -> rquickjs::Result<bool> {
                let rt = JsRuntime::upgrade(&ctx, &weak)?;
                let bucket = bucket_id(&ctx, &bucket, "GetRoutingBucketPopulationEnabled")?;
                Ok(server(&ctx, &*rt.host(&ctx)?)?.routing_bucket_population_enabled(bucket))
            },
        )?
    };
    let set_lockdown = {
        let weak = weak.clone();
        Function::new(
            ctx.clone(),
            move |ctx: Ctx<'js>, bucket: JsValue<'js>, mode: String| -> rquickjs::Result<()> {
                let rt = JsRuntime::upgrade(&ctx, &weak)?;
                let bucket = bucket_id(&ctx, &bucket, "SetRoutingBucketEntityLockdownMode")?;
                server(&ctx, &*rt.host(&ctx)?)?
                    .set_routing_bucket_entity_lockdown_mode(bucket, &mode)
                    .map_err(|err| Exception::throw_message(&ctx, &err.to_string()))
            },
        )?
    };
    let lockdown = {
        let weak = weak.clone();
        Function::new(
            ctx.clone(),
            move |ctx: Ctx<'js>, bucket: JsValue<'js>| -> rquickjs::Result<String> {
                let rt = JsRuntime::upgrade(&ctx, &weak)?;
                let bucket = bucket_id(&ctx, &bucket, "GetRoutingBucketEntityLockdownMode")?;
                Ok(server(&ctx, &*rt.host(&ctx)?)?.routing_bucket_entity_lockdown_mode(bucket))
            },
        )?
    };
    Ok(vec![
        ("SetRoutingBucketPopulationEnabled", set_population),
        ("GetRoutingBucketPopulationEnabled", population),
        ("SetRoutingBucketEntityLockdownMode", set_lockdown),
        ("GetRoutingBucketEntityLockdownMode", lockdown),
    ])
}

fn list_functions<'js>(
    ctx: &Ctx<'js>,
    weak: &Weak<JsRuntime>,
) -> rquickjs::Result<Vec<(&'static str, Function<'js>)>> {
    Ok(vec![
        ("GetAllVehicles", ids_function(ctx, weak, EntityKind::Vehicle)?),
        ("GetAllPeds", ids_function(ctx, weak, EntityKind::Ped)?),
        ("GetAllObjects", ids_function(ctx, weak, EntityKind::Object)?),
    ])
}

fn ids_function<'js>(
    ctx: &Ctx<'js>,
    weak: &Weak<JsRuntime>,
    kind: EntityKind,
) -> rquickjs::Result<Function<'js>> {
    let weak = weak.clone();
    Function::new(ctx.clone(), move |ctx: Ctx<'js>| -> rquickjs::Result<Vec<i64>> {
        let rt = JsRuntime::upgrade(&ctx, &weak)?;
        let ids = rt.host(&ctx)?.entities(Some(kind));
        Ok(ids.into_iter().map(i64::from).collect())
    })
}

fn bucket_id(ctx: &Ctx<'_>, value: &JsValue<'_>, function: &str) -> rquickjs::Result<u32> {
    whole_number(value)
        .and_then(|n| u32::try_from(n).ok())
        .ok_or_else(|| {
            Exception::throw_range(
                ctx,
                &format!("{function}: the bucket must be a whole number from 0 to 4294967295"),
            )
        })
}

fn existing(
    rt: &JsRuntime,
    ctx: &Ctx<'_>,
    value: &JsValue<'_>,
) -> rquickjs::Result<Option<(u32, EntityInfo)>> {
    let Some(id) = entity_id(value) else {
        return Ok(None);
    };
    Ok(rt.host(ctx)?.entity(id).map(|info| (id, info)))
}

fn spawn(
    rt: &JsRuntime,
    ctx: &Ctx<'_>,
    kind: EntityKind,
    model: &JsValue<'_>,
    position: [f32; 3],
    rotation: [f32; 3],
) -> rquickjs::Result<i64> {
    let info = EntityInfo {
        kind,
        model: model_of(ctx, model)?,
        position,
        rotation,
        owner: None,
    };
    let host = rt.host(ctx)?;
    let id = server(ctx, &*host)?
        .create_entity(info)
        .map_err(|err| Exception::throw_message(ctx, &err.to_string()))?;
    Ok(i64::from(id))
}

fn client_spawn(
    rt: &JsRuntime,
    ctx: &Ctx<'_>,
    kind: EntityKind,
    model: &JsValue<'_>,
    position: [f32; 3],
    rotation: [f32; 3],
    network: Option<&JsValue<'_>>,
) -> rquickjs::Result<i64> {
    if !matches!(network.and_then(JsValue::as_bool), Some(true)) {
        return Err(Exception::throw_message(
            ctx,
            "a client creates only networked entities: pass isNetwork = true",
        ));
    }
    let model = model_of(ctx, model)?;
    let id = client(ctx, &*rt.host(ctx)?)?
        .create_network_entity(kind, model, position, rotation)
        .map_err(|err| Exception::throw_message(ctx, &err.to_string()))?;
    Ok(i64::from(id))
}

fn move_entity(
    rt: &JsRuntime,
    ctx: &Ctx<'_>,
    entity: &JsValue<'_>,
    transform: Transform,
    apply: Apply,
) -> rquickjs::Result<()> {
    match entity_id(entity) {
        Some(id) => apply(rt, ctx, id, transform),
        None => Ok(()),
    }
}

fn server_move(
    rt: &JsRuntime,
    ctx: &Ctx<'_>,
    id: u32,
    transform: Transform,
) -> rquickjs::Result<()> {
    let host = rt.host(ctx)?;
    if host.entity(id).is_none() {
        return Ok(());
    }
    let srv = server(ctx, &*host)?;
    match transform {
        Transform::Position(pos) => srv.set_entity_position(id, pos),
        Transform::Rotation(rot) => srv.set_entity_rotation(id, rot),
    }
    .map_err(|err| Exception::throw_message(ctx, &err.to_string()))
}

fn client_move(
    rt: &JsRuntime,
    ctx: &Ctx<'_>,
    id: u32,
    transform: Transform,
) -> rquickjs::Result<()> {
    let host = rt.host(ctx)?;
    let owner_host = client(ctx, &*host)?;
    let (position, rotation) = match transform {
        Transform::Position(pos) => (Some(pos), None),
        Transform::Rotation(rot) => (None, Some(rot)),
    };
    match owner_host.move_owned_entity(id, position, rotation) {
        Ok(()) => Ok(()),
        Err(err) => {
            let owned =
                host.entity(id).and_then(|info| info.owner) == Some(owner_host.local_player());
            if owned {
                Err(Exception::throw_message(ctx, &err.to_string()))
            } else {
                Ok(())
            }
        }
    }
}

fn model_of(ctx: &Ctx<'_>, value: &JsValue<'_>) -> rquickjs::Result<u32> {
    if let Some(text) = value.as_string() {
        let text = text.to_string()?;
        return Ok(joaat(text.as_bytes()));
    }
    whole_number(value)
        .map(model_bits)
        .ok_or_else(|| Exception::throw_type(ctx, "the model must be a number or a string"))
}

#[expect(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    reason = "a model hash is 32 bits, and scripts may pass it as a signed integer"
)]
fn model_bits(number: i64) -> u32 {
    number as u32
}

#[expect(
    clippy::cast_possible_wrap,
    reason = "GetHashKey returns the hash as a signed 32-bit integer"
)]
fn signed_hash(model: u32) -> i64 {
    i64::from(model as i32)
}

fn entity_id(value: &JsValue<'_>) -> Option<u32> {
    whole_number(value).and_then(|n| u32::try_from(n).ok())
}

fn real(ctx: &Ctx<'_>, value: &JsValue<'_>, what: &str) -> rquickjs::Result<f32> {
    match number_of(value) {
        Some(number) if number.is_finite() => Ok(number),
        Some(_) => Err(Exception::throw_range(
            ctx,
            &format!("{what} must be a finite number"),
        )),
        None => Err(Exception::throw_type(
            ctx,
            &format!("{what} must be a number"),
        )),
    }
}

fn coordinates(
    ctx: &Ctx<'_>,
    x: &JsValue<'_>,
    y: &JsValue<'_>,
    z: &JsValue<'_>,
) -> rquickjs::Result<[f32; 3]> {
    Ok([
        real(ctx, x, "x")?,
        real(ctx, y, "y")?,
        real(ctx, z, "z")?,
    ])
}

fn invalid_entity(ctx: &Ctx<'_>, function: &str) -> rquickjs::Error {
    Exception::throw_type(ctx, &format!("{function}: expected an entity id"))
}
