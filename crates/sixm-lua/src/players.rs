//! Server-only player functions: client events, the connected-player list,
//! names, endpoints, identifiers, drops, access control checks and `Player(id)`.

use std::rc::Weak;

use mlua::{Function, Lua, MultiValue, Table, Value as LuaValue};
use sixm_core::{ClientTarget, Host, PlayerInfo, ServerHost, player_bag};

use crate::runtime::{LuaRuntime, bind_fn, lua_err};
use crate::state::state_bag;

/// Reason given to connected players when a script drops them without one.
const DEFAULT_DROP_REASON: &str = "Dropped by server";

/// Largest magnitude below which every whole `f64` is exact, so casting it to `i64` loses nothing.
const EXACT_FLOAT_LIMIT: f64 = 9_007_199_254_740_992.0;

/// Installs `TriggerClientEvent`, `GetPlayers`, `GetNumPlayerIndices`,
/// `GetPlayerName`, `GetPlayerPed`, `GetPlayerEndpoint`, the identifier functions, `DropPlayer`,
/// `IsPlayerAceAllowed`, `IsPrincipalAceAllowed` and `Player`. Only server
/// runtimes call this.
pub(crate) fn install(lua: &Lua, weak: &Weak<LuaRuntime>, globals: &Table) -> mlua::Result<()> {
    install_event_senders(lua, weak, globals)?;
    let get_players = bind_fn(lua, weak, |rt, (): ()| {
        let ids = server(&*rt.host()?)?.players();
        rt.lua.create_sequence_from(ids.iter().map(u32::to_string))
    })?;
    let get_num_player_indices = bind_fn(lua, weak, |rt, (): ()| {
        let count = server(&*rt.host()?)?.players().len();
        i64::try_from(count).map_err(mlua::Error::runtime)
    })?;
    let get_player_ped = player_ped_function(lua, weak)?;
    let get_player_name = bind_fn(lua, weak, |rt, (player,): (LuaValue,)| {
        Ok(player_info(rt, &player)?.map(|info| info.name))
    })?;
    let get_player_ping = bind_fn(lua, weak, |rt, (player,): (LuaValue,)| {
        let Some(id) = player_id(&player) else {
            return Ok(-1_i64);
        };
        let ping = server(&*rt.host()?)?.player_ping(id);
        Ok(ping.map_or(-1, |duration| {
            i64::try_from(duration.as_millis()).unwrap_or(i64::MAX)
        }))
    })?;
    let get_player_endpoint = bind_fn(lua, weak, |rt, (player,): (LuaValue,)| {
        Ok(player_info(rt, &player)?.map(|info| info.endpoint))
    })?;
    let get_player_identifiers = bind_fn(lua, weak, |rt, (player,): (LuaValue,)| {
        let identifiers = identifiers_of_player(rt, &player)?;
        rt.lua.create_sequence_from(identifiers)
    })?;
    let get_num_player_identifiers = bind_fn(lua, weak, |rt, (player,): (LuaValue,)| {
        let count = identifiers_of_player(rt, &player)?.len();
        i64::try_from(count).map_err(mlua::Error::runtime)
    })?;
    let get_player_identifier = bind_fn(lua, weak, |rt, (player, index): (LuaValue, LuaValue)| {
        let identifiers = identifiers_of_player(rt, &player)?;
        let index = whole_number(&index).and_then(|index| usize::try_from(index).ok());
        Ok(index.and_then(|index| identifiers.get(index).cloned()))
    })?;
    let get_player_identifier_by_type =
        bind_fn(lua, weak, |rt, (player, kind): (LuaValue, String)| {
            let identifiers = identifiers_of_player(rt, &player)?;
            Ok(identifiers
                .into_iter()
                .find(|identifier| identifier_kind(identifier) == Some(kind.as_str())))
        })?;
    let drop_player = bind_fn(
        lua,
        weak,
        |rt, (player, reason): (LuaValue, Option<String>)| {
            let id = player_id(&player).ok_or_else(|| invalid_player("DropPlayer"))?;
            let reason = reason.as_deref().unwrap_or(DEFAULT_DROP_REASON);
            server(&*rt.host()?)?.drop_player(id, reason);
            Ok(())
        },
    )?;
    let is_player_ace_allowed = bind_fn(lua, weak, |rt, (player, object): (LuaValue, String)| {
        let Some(id) = player_id(&player) else {
            return Ok(false);
        };
        let host = rt.host()?;
        let server = server(&*host)?;
        if server.player_info(id).is_none() {
            return Ok(false);
        }
        Ok(server.is_ace_allowed(&format!("player.{id}"), &object))
    })?;
    let is_principal_ace_allowed =
        bind_fn(lua, weak, |rt, (principal, object): (String, String)| {
            Ok(server(&*rt.host()?)?.is_ace_allowed(&principal, &object))
        })?;
    let player = bind_fn(lua, weak, |rt, (id,): (LuaValue,)| {
        let id = player_id(&id).ok_or_else(|| invalid_player("Player"))?;
        let object = rt.lua.create_table()?;
        object.raw_set("state", state_bag(rt, &player_bag(id))?)?;
        Ok(object)
    })?;

    let functions = [
        ("GetPlayers", get_players),
        ("GetNumPlayerIndices", get_num_player_indices),
        ("GetPlayerName", get_player_name),
        ("GetPlayerPing", get_player_ping),
        ("GetPlayerPed", get_player_ped),
        ("GetPlayerEndpoint", get_player_endpoint),
        ("GetPlayerIdentifiers", get_player_identifiers),
        ("GetNumPlayerIdentifiers", get_num_player_identifiers),
        ("GetPlayerIdentifier", get_player_identifier),
        ("GetPlayerIdentifierByType", get_player_identifier_by_type),
        ("DropPlayer", drop_player),
        ("IsPlayerAceAllowed", is_player_ace_allowed),
        ("IsPrincipalAceAllowed", is_principal_ace_allowed),
        ("Player", player),
    ];
    for (name, function) in functions {
        globals.raw_set(name, function)?;
    }
    Ok(())
}

fn install_event_senders(lua: &Lua, weak: &Weak<LuaRuntime>, globals: &Table) -> mlua::Result<()> {
    let trigger_client_event = bind_fn(
        lua,
        weak,
        |rt, (name, target, args): (String, LuaValue, MultiValue)| {
            let target = client_target(&target)?;
            let values = rt.to_network_values(&args.into_vec()).map_err(lua_err)?;
            let host = rt.host()?;
            server(&*host)?
                .trigger_client_event(&name, target, &values)
                .map_err(lua_err)
        },
    )?;
    let trigger_latent_client_event = bind_fn(
        lua,
        weak,
        |rt, (name, target, bps, args): (String, LuaValue, LuaValue, MultiValue)| {
            let target = client_target(&target)?;
            let bps = crate::client::latent_bps(&bps)?;
            let values = rt.to_network_values(&args.into_vec()).map_err(lua_err)?;
            let host = rt.host()?;
            server(&*host)?
                .trigger_latent_client_event(&name, target, &values, bps)
                .map_err(lua_err)
        },
    )?;
    globals.raw_set("TriggerClientEvent", trigger_client_event)?;
    globals.raw_set("TriggerLatentClientEvent", trigger_latent_client_event)
}

/// `GetPlayerPed`: the ped of a connected player, or 0 for a player without one.
fn player_ped_function(lua: &Lua, weak: &Weak<LuaRuntime>) -> mlua::Result<Function> {
    bind_fn(lua, weak, |rt, (player,): (LuaValue,)| {
        let host = rt.host()?;
        let ped = match player_id(&player) {
            Some(id) => server(&*host)?.player_ped(id),
            None => None,
        };
        Ok(i64::from(ped.unwrap_or(0)))
    })
}

/// The server services of `host`. Only server runtimes call the functions that
/// use this, so a missing service is a broken host rather than a script error.
pub(crate) fn server(host: &dyn Host) -> mlua::Result<&dyn ServerHost> {
    host.server()
        .ok_or_else(|| mlua::Error::runtime("this function needs a server host"))
}

/// The player `player` names, or `None` when it names no connected player.
fn player_info(rt: &LuaRuntime, player: &LuaValue) -> mlua::Result<Option<PlayerInfo>> {
    let Some(id) = player_id(player) else {
        return Ok(None);
    };
    Ok(server(&*rt.host()?)?.player_info(id))
}

/// The identifiers of the player `player` names. Empty when it names no player.
fn identifiers_of_player(rt: &LuaRuntime, player: &LuaValue) -> mlua::Result<Vec<String>> {
    let Some(id) = player_id(player) else {
        return Ok(Vec::new());
    };
    Ok(server(&*rt.host()?)?.player_identifiers(id))
}

/// The kind of an identifier: the text before its first `:`, as in `license`
/// for `license:<hex>`.
fn identifier_kind(identifier: &str) -> Option<&str> {
    identifier.split_once(':').map(|(kind, _)| kind)
}

/// Target of `TriggerClientEvent`: `-1` addresses every player.
fn client_target(value: &LuaValue) -> mlua::Result<ClientTarget> {
    match whole_number(value) {
        Some(-1) => Ok(ClientTarget::All),
        number => number
            .and_then(|number| u32::try_from(number).ok())
            .map(ClientTarget::Player)
            .ok_or_else(|| invalid_player("TriggerClientEvent")),
    }
}

/// A server id for a player argument, or `None` when it is not one.
pub(crate) fn player_id(value: &LuaValue) -> Option<u32> {
    whole_number(value).and_then(|number| u32::try_from(number).ok())
}

/// Reads an integer, a float with no fraction, or a decimal string as an integer.
pub(crate) fn whole_number(value: &LuaValue) -> Option<i64> {
    match value {
        LuaValue::Integer(int) => Some(*int),
        LuaValue::Number(float) => whole_float(*float),
        LuaValue::String(text) => text.to_str().ok()?.parse().ok(),
        _ => None,
    }
}

#[expect(
    clippy::cast_possible_truncation,
    reason = "the value is checked to be a whole number within the exact range first"
)]
fn whole_float(float: f64) -> Option<i64> {
    (float.fract() == 0.0 && float.abs() <= EXACT_FLOAT_LIMIT).then_some(float as i64)
}

pub(crate) fn invalid_player(function: &str) -> mlua::Error {
    mlua::Error::runtime(format!("{function}: expected a player id"))
}
