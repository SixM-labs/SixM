//! Client-only globals: the connection to the server and the local player.

use std::rc::Rc;

use mlua::{MultiValue, Table, Value as LuaValue};
use sixm_core::{ClientHost, Host, player_bag};

use crate::players::{player_id, whole_number};
use crate::runtime::{LuaRuntime, bind_fn, lua_err};
use crate::state::state_bag;

/// Player handle of the local player. Remote players are not tracked yet, so
/// the local player is the only one a script can name.
const LOCAL_PLAYER_HANDLE: u32 = 0;

/// Installs `TriggerServerEvent`, `PlayerId`, `GetPlayerServerId`,
/// `PlayerPedId`, `GetPlayerPed` and `LocalPlayer`. Only client runtimes call
/// this, with the client's own server id, which the runtime read from its host
/// when it was created.
pub(crate) fn install(
    runtime: &Rc<LuaRuntime>,
    globals: &Table,
    local_player: u32,
) -> mlua::Result<()> {
    let lua = &runtime.lua;
    let weak = &runtime.me;
    let trigger_server_event = bind_fn(lua, weak, |rt, (name, args): (String, MultiValue)| {
        let values = rt.to_network_values(&args.into_vec()).map_err(lua_err)?;
        let host = rt.host()?;
        client(&*host)?
            .trigger_server_event(&name, &values)
            .map_err(lua_err)
    })?;
    let trigger_latent_server_event = bind_fn(
        lua,
        weak,
        |rt, (name, bps, args): (String, LuaValue, MultiValue)| {
            let bps = latent_bps(&bps)?;
            let values = rt.to_network_values(&args.into_vec()).map_err(lua_err)?;
            let host = rt.host()?;
            client(&*host)?
                .trigger_latent_server_event(&name, &values, bps)
                .map_err(lua_err)
        },
    )?;
    let player_id_function = bind_fn(lua, weak, |_, (): ()| Ok(LOCAL_PLAYER_HANDLE))?;
    let get_player_server_id = bind_fn(lua, weak, move |_, (player,): (mlua::Value,)| {
        Ok((player_id(&player) == Some(LOCAL_PLAYER_HANDLE)).then_some(local_player))
    })?;
    let player_ped_id = bind_fn(lua, weak, |rt, (): ()| local_ped(rt))?;
    let get_player_ped = bind_fn(lua, weak, |rt, (player,): (mlua::Value,)| {
        // Only the local player is known, so it is named by its handle or by -1.
        let is_local =
            whole_number(&player) == Some(-1) || player_id(&player) == Some(LOCAL_PLAYER_HANDLE);
        if is_local { local_ped(rt) } else { Ok(0) }
    })?;

    let local = lua.create_table()?;
    local.raw_set("state", state_bag(runtime, &player_bag(local_player))?)?;

    let functions = [
        ("TriggerServerEvent", trigger_server_event),
        ("TriggerLatentServerEvent", trigger_latent_server_event),
        ("PlayerId", player_id_function),
        ("GetPlayerServerId", get_player_server_id),
        ("PlayerPedId", player_ped_id),
        ("GetPlayerPed", get_player_ped),
    ];
    for (name, function) in functions {
        globals.raw_set(name, function)?;
    }
    globals.raw_set("LocalPlayer", local)?;
    Ok(())
}

pub(crate) fn latent_bps(value: &LuaValue) -> mlua::Result<u64> {
    if matches!(value, LuaValue::Number(rate) if rate.is_finite() && *rate <= 0.0) {
        return Ok(25_000);
    }
    if !matches!(value, LuaValue::Integer(_) | LuaValue::Number(_)) {
        return Err(mlua::Error::runtime(
            "expected a numeric bytes-per-second rate",
        ));
    }
    let rate = whole_number(value)
        .ok_or_else(|| mlua::Error::runtime("expected an exactly representable integer rate"))?;
    Ok(if rate > 0 {
        rate.unsigned_abs()
    } else {
        25_000
    })
}

/// The id of the local player's ped, or 0 until the server has sent it.
fn local_ped(rt: &LuaRuntime) -> mlua::Result<i64> {
    let host = rt.host()?;
    Ok(i64::from(client(&*host)?.local_ped().unwrap_or(0)))
}

/// The client services of `host`. Only client runtimes call the functions that
/// use this, so a missing service is a broken host rather than a script error.
pub(crate) fn client(host: &dyn Host) -> mlua::Result<&dyn ClientHost> {
    host.client()
        .ok_or_else(|| mlua::Error::runtime("this function needs a client host"))
}
