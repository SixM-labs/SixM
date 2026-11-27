//! Server-only player functions: `emitNet` / `TriggerClientEvent`,
//! `TriggerLatentClientEvent`, `getPlayers` / `GetPlayers`, `GetNumPlayerIndices`,
//! `DoesPlayerExist`, `GetPlayerName`, `GetPlayerPing`, `GetPlayerPed`,
//! `GetPlayerEndpoint`, identifier natives, `GetPlayerGuid`, `GetPlayerTimeOnline`,
//! `GetPlayerFromIndex`, `GetHostId`, `DropPlayer`, ACE checks, and `Player(id)`.

use std::rc::Weak;

use rquickjs::function::{Opt, Rest};
use rquickjs::{Ctx, Exception, Function, Object, Value as JsValue};
use sixm_core::{ClientTarget, Host, PlayerInfo, ServerHost, player_bag};

use crate::client::latent_bps;
use crate::convert::exact_safe_int;
use crate::runtime::JsRuntime;
use crate::state::state_bag;

const DEFAULT_DROP_REASON: &str = "Dropped by server";

pub(crate) fn install(ctx: &Ctx<'_>, weak: &Weak<JsRuntime>) -> rquickjs::Result<()> {
    install_event_senders(ctx, weak)?;
    install_roster(ctx, weak)?;
    install_identifiers(ctx, weak)?;
    install_connection_info(ctx, weak)?;
    install_tokens_and_last_msg(ctx, weak)?;
    install_acl_and_player(ctx, weak)?;
    Ok(())
}

fn install_roster<'js>(ctx: &Ctx<'js>, weak: &Weak<JsRuntime>) -> rquickjs::Result<()> {
    let globals = ctx.globals();
    let get_players = {
        let weak = weak.clone();
        Function::new(
            ctx.clone(),
            move |ctx: Ctx<'js>| -> rquickjs::Result<Vec<String>> {
                let rt = JsRuntime::upgrade(&ctx, &weak)?;
                let ids = server(&ctx, &*rt.host(&ctx)?)?.players();
                Ok(ids.iter().map(u32::to_string).collect())
            },
        )?
    };
    let get_num_player_indices = {
        let weak = weak.clone();
        Function::new(ctx.clone(), move |ctx: Ctx<'js>| -> rquickjs::Result<i64> {
            let rt = JsRuntime::upgrade(&ctx, &weak)?;
            let count = server(&ctx, &*rt.host(&ctx)?)?.players().len();
            i64::try_from(count).map_err(|err| Exception::throw_message(&ctx, &err.to_string()))
        })?
    };
    let get_player_ped = {
        let weak = weak.clone();
        Function::new(
            ctx.clone(),
            move |ctx: Ctx<'js>, player: JsValue<'js>| -> rquickjs::Result<i64> {
                let rt = JsRuntime::upgrade(&ctx, &weak)?;
                let host = rt.host(&ctx)?;
                let ped = match player_id(&player) {
                    Some(id) => server(&ctx, &*host)?.player_ped(id),
                    None => None,
                };
                Ok(i64::from(ped.unwrap_or(0)))
            },
        )?
    };
    let get_player_name = {
        let weak = weak.clone();
        Function::new(
            ctx.clone(),
            move |ctx: Ctx<'js>, player: JsValue<'js>| -> rquickjs::Result<Option<String>> {
                let rt = JsRuntime::upgrade(&ctx, &weak)?;
                Ok(player_info(&rt, &ctx, &player)?.map(|info| info.name))
            },
        )?
    };
    let get_player_ping = {
        let weak = weak.clone();
        Function::new(
            ctx.clone(),
            move |ctx: Ctx<'js>, player: JsValue<'js>| -> rquickjs::Result<i64> {
                let rt = JsRuntime::upgrade(&ctx, &weak)?;
                let Some(id) = player_id(&player) else {
                    return Ok(-1);
                };
                let ping = server(&ctx, &*rt.host(&ctx)?)?.player_ping(id);
                Ok(ping.map_or(-1, |d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX)))
            },
        )?
    };
    let get_player_endpoint = {
        let weak = weak.clone();
        Function::new(
            ctx.clone(),
            move |ctx: Ctx<'js>, player: JsValue<'js>| -> rquickjs::Result<Option<String>> {
                let rt = JsRuntime::upgrade(&ctx, &weak)?;
                Ok(player_info(&rt, &ctx, &player)?.map(|info| info.endpoint))
            },
        )?
    };
    let does_player_exist = {
        let weak = weak.clone();
        Function::new(
            ctx.clone(),
            move |ctx: Ctx<'js>, player: JsValue<'js>| -> rquickjs::Result<bool> {
                let rt = JsRuntime::upgrade(&ctx, &weak)?;
                let Some(id) = player_id(&player) else {
                    return Ok(false);
                };
                Ok(server(&ctx, &*rt.host(&ctx)?)?.player_info(id).is_some())
            },
        )?
    };
    for (name, func) in [
        ("getPlayers", get_players.clone()),
        ("GetPlayers", get_players),
        ("GetNumPlayerIndices", get_num_player_indices),
        ("DoesPlayerExist", does_player_exist),
        ("GetPlayerName", get_player_name),
        ("GetPlayerPing", get_player_ping),
        ("GetPlayerPed", get_player_ped),
        ("GetPlayerEndpoint", get_player_endpoint),
    ] {
        globals.set(name, func)?;
    }
    Ok(())
}

fn install_identifiers<'js>(ctx: &Ctx<'js>, weak: &Weak<JsRuntime>) -> rquickjs::Result<()> {
    let globals = ctx.globals();
    let get_player_identifiers = {
        let weak = weak.clone();
        Function::new(
            ctx.clone(),
            move |ctx: Ctx<'js>, player: JsValue<'js>| -> rquickjs::Result<Vec<String>> {
                let rt = JsRuntime::upgrade(&ctx, &weak)?;
                identifiers_of_player(&rt, &ctx, &player)
            },
        )?
    };
    let get_num_player_identifiers = {
        let weak = weak.clone();
        Function::new(
            ctx.clone(),
            move |ctx: Ctx<'js>, player: JsValue<'js>| -> rquickjs::Result<i64> {
                let rt = JsRuntime::upgrade(&ctx, &weak)?;
                let count = identifiers_of_player(&rt, &ctx, &player)?.len();
                i64::try_from(count).map_err(|err| Exception::throw_message(&ctx, &err.to_string()))
            },
        )?
    };
    let get_player_identifier = {
        let weak = weak.clone();
        Function::new(
            ctx.clone(),
            move |ctx: Ctx<'js>,
                  player: JsValue<'js>,
                  index: JsValue<'js>|
                  -> rquickjs::Result<Option<String>> {
                let rt = JsRuntime::upgrade(&ctx, &weak)?;
                let identifiers = identifiers_of_player(&rt, &ctx, &player)?;
                let index = whole_number(&index).and_then(|i| usize::try_from(i).ok());
                Ok(index.and_then(|i| identifiers.get(i).cloned()))
            },
        )?
    };
    let get_player_identifier_by_type = {
        let weak = weak.clone();
        Function::new(
            ctx.clone(),
            move |ctx: Ctx<'js>,
                  player: JsValue<'js>,
                  kind: String|
                  -> rquickjs::Result<Option<String>> {
                let rt = JsRuntime::upgrade(&ctx, &weak)?;
                let identifiers = identifiers_of_player(&rt, &ctx, &player)?;
                Ok(identifiers
                    .into_iter()
                    .find(|identifier| identifier_kind(identifier) == Some(kind.as_str())))
            },
        )?
    };
    let get_player_guid = {
        let weak = weak.clone();
        Function::new(
            ctx.clone(),
            move |ctx: Ctx<'js>, player: JsValue<'js>| -> rquickjs::Result<Option<String>> {
                let rt = JsRuntime::upgrade(&ctx, &weak)?;
                let identifiers = identifiers_of_player(&rt, &ctx, &player)?;
                Ok(identifiers
                    .iter()
                    .find(|identifier| identifier_kind(identifier) == Some("license"))
                    .and_then(|identifier| identifier.split_once(':'))
                    .map(|(_, guid)| guid.to_owned()))
            },
        )?
    };
    for (name, func) in [
        ("GetPlayerIdentifiers", get_player_identifiers),
        ("GetNumPlayerIdentifiers", get_num_player_identifiers),
        ("GetPlayerIdentifier", get_player_identifier),
        ("GetPlayerIdentifierByType", get_player_identifier_by_type),
        ("GetPlayerGuid", get_player_guid),
    ] {
        globals.set(name, func)?;
    }
    Ok(())
}

fn install_connection_info<'js>(ctx: &Ctx<'js>, weak: &Weak<JsRuntime>) -> rquickjs::Result<()> {
    let globals = ctx.globals();
    let get_player_time_online = {
        let weak = weak.clone();
        Function::new(
            ctx.clone(),
            move |ctx: Ctx<'js>, player: JsValue<'js>| -> rquickjs::Result<Option<i64>> {
                let rt = JsRuntime::upgrade(&ctx, &weak)?;
                let Some(id) = player_id(&player) else {
                    return Ok(None);
                };
                let online = server(&ctx, &*rt.host(&ctx)?)?.player_time_online(id);
                online
                    .map(|d| {
                        i64::try_from(d.as_secs())
                            .map_err(|err| Exception::throw_message(&ctx, &err.to_string()))
                    })
                    .transpose()
            },
        )?
    };
    let get_player_from_index = {
        let weak = weak.clone();
        Function::new(
            ctx.clone(),
            move |ctx: Ctx<'js>, index: JsValue<'js>| -> rquickjs::Result<Option<String>> {
                let rt = JsRuntime::upgrade(&ctx, &weak)?;
                let players = server(&ctx, &*rt.host(&ctx)?)?.players();
                let index = whole_number(&index).and_then(|i| usize::try_from(i).ok());
                Ok(index.and_then(|i| players.get(i).map(u32::to_string)))
            },
        )?
    };
    let get_host_id = {
        let weak = weak.clone();
        Function::new(
            ctx.clone(),
            move |ctx: Ctx<'js>| -> rquickjs::Result<Option<String>> {
                let rt = JsRuntime::upgrade(&ctx, &weak)?;
                let players = server(&ctx, &*rt.host(&ctx)?)?.players();
                Ok(players.iter().min().map(u32::to_string))
            },
        )?
    };
    for (name, func) in [
        ("GetPlayerTimeOnline", get_player_time_online),
        ("GetPlayerFromIndex", get_player_from_index),
        ("GetHostId", get_host_id),
    ] {
        globals.set(name, func)?;
    }
    Ok(())
}

fn install_tokens_and_last_msg<'js>(
    ctx: &Ctx<'js>,
    weak: &Weak<JsRuntime>,
) -> rquickjs::Result<()> {
    let globals = ctx.globals();
    let get_player_last_msg = {
        let weak = weak.clone();
        Function::new(
            ctx.clone(),
            move |ctx: Ctx<'js>, player: JsValue<'js>| -> rquickjs::Result<i64> {
                let rt = JsRuntime::upgrade(&ctx, &weak)?;
                let Some(id) = player_id(&player) else {
                    return Ok(-1);
                };
                let msg = server(&ctx, &*rt.host(&ctx)?)?.player_last_msg(id);
                Ok(msg.map_or(-1, |d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX)))
            },
        )?
    };
    let get_num_player_tokens = {
        let weak = weak.clone();
        Function::new(
            ctx.clone(),
            move |ctx: Ctx<'js>, player: JsValue<'js>| -> rquickjs::Result<i64> {
                let rt = JsRuntime::upgrade(&ctx, &weak)?;
                let Some(id) = player_id(&player) else {
                    return Ok(0);
                };
                let count = server(&ctx, &*rt.host(&ctx)?)?.player_tokens(id).len();
                i64::try_from(count).map_err(|err| Exception::throw_message(&ctx, &err.to_string()))
            },
        )?
    };
    let get_player_token = {
        let weak = weak.clone();
        Function::new(
            ctx.clone(),
            move |ctx: Ctx<'js>,
                  player: JsValue<'js>,
                  index: JsValue<'js>|
                  -> rquickjs::Result<Option<String>> {
                let rt = JsRuntime::upgrade(&ctx, &weak)?;
                let Some(id) = player_id(&player) else {
                    return Ok(None);
                };
                let tokens = server(&ctx, &*rt.host(&ctx)?)?.player_tokens(id);
                let idx = whole_number(&index).and_then(|i| usize::try_from(i).ok());
                Ok(idx.and_then(|i| tokens.get(i).cloned()))
            },
        )?
    };
    for (name, func) in [
        ("GetPlayerLastMsg", get_player_last_msg),
        ("GetNumPlayerTokens", get_num_player_tokens),
        ("GetPlayerToken", get_player_token),
    ] {
        globals.set(name, func)?;
    }
    Ok(())
}

fn install_acl_and_player<'js>(ctx: &Ctx<'js>, weak: &Weak<JsRuntime>) -> rquickjs::Result<()> {
    let globals = ctx.globals();
    let drop_player = {
        let weak = weak.clone();
        Function::new(
            ctx.clone(),
            move |ctx: Ctx<'js>,
                  player: JsValue<'js>,
                  reason: Opt<Option<String>>|
                  -> rquickjs::Result<()> {
                let rt = JsRuntime::upgrade(&ctx, &weak)?;
                let id = player_id(&player).ok_or_else(|| invalid_player(&ctx, "DropPlayer"))?;
                let reason = reason.0.flatten();
                let reason = reason.as_deref().unwrap_or(DEFAULT_DROP_REASON);
                server(&ctx, &*rt.host(&ctx)?)?.drop_player(id, reason);
                Ok(())
            },
        )?
    };
    let is_ace_allowed = {
        let weak = weak.clone();
        Function::new(
            ctx.clone(),
            move |ctx: Ctx<'js>, object: String| -> rquickjs::Result<bool> {
                let rt = JsRuntime::upgrade(&ctx, &weak)?;
                let principal = format!("resource.{}", rt.resource);
                Ok(server(&ctx, &*rt.host(&ctx)?)?.is_ace_allowed(&principal, &object))
            },
        )?
    };
    let is_player_ace_allowed = {
        let weak = weak.clone();
        Function::new(
            ctx.clone(),
            move |ctx: Ctx<'js>, player: JsValue<'js>, object: String| -> rquickjs::Result<bool> {
                let rt = JsRuntime::upgrade(&ctx, &weak)?;
                let Some(id) = player_id(&player) else {
                    return Ok(false);
                };
                let host = rt.host(&ctx)?;
                let srv = server(&ctx, &*host)?;
                if srv.player_info(id).is_none() {
                    return Ok(false);
                }
                Ok(srv.is_ace_allowed(&format!("player.{id}"), &object))
            },
        )?
    };
    let is_principal_ace_allowed = {
        let weak = weak.clone();
        Function::new(
            ctx.clone(),
            move |ctx: Ctx<'js>, principal: String, object: String| -> rquickjs::Result<bool> {
                let rt = JsRuntime::upgrade(&ctx, &weak)?;
                Ok(server(&ctx, &*rt.host(&ctx)?)?.is_ace_allowed(&principal, &object))
            },
        )?
    };
    let player = {
        let weak = weak.clone();
        Function::new(
            ctx.clone(),
            move |ctx: Ctx<'js>, id: JsValue<'js>| -> rquickjs::Result<Object<'js>> {
                let _ = JsRuntime::upgrade(&ctx, &weak)?;
                let id = player_id(&id).ok_or_else(|| invalid_player(&ctx, "Player"))?;
                let obj = Object::new(ctx.clone())?;
                obj.set("state", state_bag(&ctx, &weak, &player_bag(id))?)?;
                Ok(obj)
            },
        )?
    };
    for (name, func) in [
        ("DropPlayer", drop_player),
        ("IsAceAllowed", is_ace_allowed),
        ("IsPlayerAceAllowed", is_player_ace_allowed),
        ("IsPrincipalAceAllowed", is_principal_ace_allowed),
        ("Player", player),
    ] {
        globals.set(name, func)?;
    }
    Ok(())
}

fn install_event_senders<'js>(ctx: &Ctx<'js>, weak: &Weak<JsRuntime>) -> rquickjs::Result<()> {
    let globals = ctx.globals();

    let trigger_client = {
        let weak = weak.clone();
        Function::new(
            ctx.clone(),
            move |ctx: Ctx<'js>,
                  name: String,
                  target: JsValue<'js>,
                  args: Rest<JsValue<'js>>|
                  -> rquickjs::Result<()> {
                let rt = JsRuntime::upgrade(&ctx, &weak)?;
                let targets = client_targets(&ctx, &target, "TriggerClientEvent")?;
                let values = rt
                    .to_network_values(&ctx, &args.0)
                    .map_err(|err| Exception::throw_message(&ctx, &err.to_string()))?;
                let host = rt.host(&ctx)?;
                let srv = server(&ctx, &*host)?;
                for target in targets {
                    srv.trigger_client_event(&name, target, &values)
                        .map_err(|err| Exception::throw_message(&ctx, &err.to_string()))?;
                }
                Ok(())
            },
        )?
    };

    let trigger_latent_client = {
        let weak = weak.clone();
        Function::new(
            ctx.clone(),
            move |ctx: Ctx<'js>,
                  name: String,
                  target: JsValue<'js>,
                  bps: JsValue<'js>,
                  args: Rest<JsValue<'js>>|
                  -> rquickjs::Result<()> {
                let rt = JsRuntime::upgrade(&ctx, &weak)?;
                let targets = client_targets(&ctx, &target, "TriggerLatentClientEvent")?;
                let bps = latent_bps(&ctx, &bps)?;
                let values = rt
                    .to_network_values(&ctx, &args.0)
                    .map_err(|err| Exception::throw_message(&ctx, &err.to_string()))?;
                let host = rt.host(&ctx)?;
                let srv = server(&ctx, &*host)?;
                for target in targets {
                    srv.trigger_latent_client_event(&name, target, &values, bps)
                        .map_err(|err| Exception::throw_message(&ctx, &err.to_string()))?;
                }
                Ok(())
            },
        )?
    };

    globals.set("emitNet", trigger_client.clone())?;
    globals.set("TriggerClientEvent", trigger_client)?;
    globals.set("TriggerLatentClientEvent", trigger_latent_client)?;
    Ok(())
}

pub(crate) fn server<'a>(
    ctx: &Ctx<'_>,
    host: &'a dyn Host,
) -> rquickjs::Result<&'a dyn ServerHost> {
    host.server()
        .ok_or_else(|| Exception::throw_message(ctx, "this function needs a server host"))
}

fn player_info(
    rt: &JsRuntime,
    ctx: &Ctx<'_>,
    player: &JsValue<'_>,
) -> rquickjs::Result<Option<PlayerInfo>> {
    let Some(id) = player_id(player) else {
        return Ok(None);
    };
    Ok(server(ctx, &*rt.host(ctx)?)?.player_info(id))
}

fn identifiers_of_player(
    rt: &JsRuntime,
    ctx: &Ctx<'_>,
    player: &JsValue<'_>,
) -> rquickjs::Result<Vec<String>> {
    let Some(id) = player_id(player) else {
        return Ok(Vec::new());
    };
    Ok(server(ctx, &*rt.host(ctx)?)?.player_identifiers(id))
}

fn identifier_kind(identifier: &str) -> Option<&str> {
    identifier.split_once(':').map(|(kind, _)| kind)
}

fn client_targets(
    ctx: &Ctx<'_>,
    value: &JsValue<'_>,
    function: &str,
) -> rquickjs::Result<Vec<ClientTarget>> {
    if let Some(arr) = value.as_array() {
        let mut targets = Vec::with_capacity(arr.len());
        for index in 0..arr.len() {
            let item: JsValue<'_> = arr.get(index)?;
            let id = player_id(&item).ok_or_else(|| invalid_player(ctx, function))?;
            targets.push(ClientTarget::Player(id));
        }
        return Ok(targets);
    }
    match whole_number(value) {
        Some(-1) => Ok(vec![ClientTarget::All]),
        number => number
            .and_then(|n| u32::try_from(n).ok())
            .map(|id| vec![ClientTarget::Player(id)])
            .ok_or_else(|| invalid_player(ctx, function)),
    }
}

pub(crate) fn player_id(value: &JsValue<'_>) -> Option<u32> {
    whole_number(value).and_then(|n| u32::try_from(n).ok())
}

/// Reads an integer, a finite whole float within safe range, or a decimal string.
pub(crate) fn whole_number(value: &JsValue<'_>) -> Option<i64> {
    if let Some(int) = value.as_int() {
        Some(i64::from(int))
    } else if let Some(float) = value.as_float() {
        exact_safe_int(float)
    } else if let Some(text) = value.as_string() {
        text.to_string().ok()?.parse::<i64>().ok()
    } else {
        None
    }
}

pub(crate) fn invalid_player(ctx: &Ctx<'_>, function: &str) -> rquickjs::Error {
    Exception::throw_type(ctx, &format!("{function}: expected a player id"))
}
