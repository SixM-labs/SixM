//! Client-only globals: `emitNet` / `TriggerServerEvent`,
//! `TriggerLatentServerEvent`, `PlayerId`, `GetPlayerServerId`,
//! `PlayerPedId`, `GetPlayerPed`, and `LocalPlayer`.

use std::rc::Weak;

use rquickjs::function::Rest;
use rquickjs::{Ctx, Exception, Function, Object, Value as JsValue};
use sixm_core::{ClientHost, Host, player_bag};

use crate::players::{player_id, whole_number};
use crate::runtime::JsRuntime;
use crate::state::state_bag;

const LOCAL_PLAYER_HANDLE: u32 = 0;

pub(crate) fn install<'js>(
    ctx: &Ctx<'js>,
    weak: &Weak<JsRuntime>,
    local_player: u32,
) -> rquickjs::Result<()> {
    let globals = ctx.globals();

    let trigger_server = {
        let weak = weak.clone();
        Function::new(
            ctx.clone(),
            move |ctx: Ctx<'js>, name: String, args: Rest<JsValue<'js>>| -> rquickjs::Result<()> {
                let rt = JsRuntime::upgrade(&ctx, &weak)?;
                let values = rt
                    .to_network_values(&ctx, &args.0)
                    .map_err(|err| Exception::throw_message(&ctx, &err.to_string()))?;
                let host = rt.host(&ctx)?;
                client(&ctx, &*host)?
                    .trigger_server_event(&name, &values)
                    .map_err(|err| Exception::throw_message(&ctx, &err.to_string()))
            },
        )?
    };

    let trigger_latent_server = {
        let weak = weak.clone();
        Function::new(
            ctx.clone(),
            move |ctx: Ctx<'js>,
                  name: String,
                  bps: JsValue<'js>,
                  args: Rest<JsValue<'js>>|
                  -> rquickjs::Result<()> {
                let rt = JsRuntime::upgrade(&ctx, &weak)?;
                let bps = latent_bps(&ctx, &bps)?;
                let values = rt
                    .to_network_values(&ctx, &args.0)
                    .map_err(|err| Exception::throw_message(&ctx, &err.to_string()))?;
                let host = rt.host(&ctx)?;
                client(&ctx, &*host)?
                    .trigger_latent_server_event(&name, &values, bps)
                    .map_err(|err| Exception::throw_message(&ctx, &err.to_string()))
            },
        )?
    };

    let player_id_fn = {
        let weak = weak.clone();
        Function::new(ctx.clone(), move |ctx: Ctx<'js>| -> rquickjs::Result<u32> {
            let _ = JsRuntime::upgrade(&ctx, &weak)?;
            Ok(LOCAL_PLAYER_HANDLE)
        })?
    };

    let get_player_server_id = {
        let weak = weak.clone();
        Function::new(
            ctx.clone(),
            move |ctx: Ctx<'js>, player: JsValue<'js>| -> rquickjs::Result<Option<u32>> {
                let _ = JsRuntime::upgrade(&ctx, &weak)?;
                Ok((player_id(&player) == Some(LOCAL_PLAYER_HANDLE)).then_some(local_player))
            },
        )?
    };

    let player_ped_id = {
        let weak = weak.clone();
        Function::new(ctx.clone(), move |ctx: Ctx<'js>| -> rquickjs::Result<i64> {
            let rt = JsRuntime::upgrade(&ctx, &weak)?;
            local_ped(&rt, &ctx)
        })?
    };

    let get_player_ped = {
        let weak = weak.clone();
        Function::new(
            ctx.clone(),
            move |ctx: Ctx<'js>, player: JsValue<'js>| -> rquickjs::Result<i64> {
                let rt = JsRuntime::upgrade(&ctx, &weak)?;
                let is_local = whole_number(&player) == Some(-1)
                    || player_id(&player) == Some(LOCAL_PLAYER_HANDLE);
                if is_local {
                    local_ped(&rt, &ctx)
                } else {
                    Ok(0)
                }
            },
        )?
    };

    let local = Object::new(ctx.clone())?;
    local.set("state", state_bag(ctx, weak, &player_bag(local_player))?)?;

    globals.set("emitNet", trigger_server.clone())?;
    globals.set("TriggerServerEvent", trigger_server)?;
    globals.set("TriggerLatentServerEvent", trigger_latent_server)?;
    globals.set("PlayerId", player_id_fn)?;
    globals.set("GetPlayerServerId", get_player_server_id)?;
    globals.set("PlayerPedId", player_ped_id)?;
    globals.set("GetPlayerPed", get_player_ped)?;
    globals.set("LocalPlayer", local)?;
    Ok(())
}

pub(crate) fn latent_bps(ctx: &Ctx<'_>, value: &JsValue<'_>) -> rquickjs::Result<u64> {
    if let Some(float) = value.as_float()
        && float.is_finite()
        && float <= 0.0
    {
        return Ok(25_000);
    }
    if let Some(int) = value.as_int() {
        return Ok(if int > 0 {
            int.unsigned_abs().into()
        } else {
            25_000
        });
    }
    if let Some(bigint) = value.as_big_int() {
        let rate = bigint
            .clone()
            .to_i64()
            .map_err(|_| Exception::throw_type(ctx, "expected an integer rate"))?;
        return Ok(if rate > 0 {
            rate.unsigned_abs()
        } else {
            25_000
        });
    }
    if !value.is_number() {
        return Err(Exception::throw_type(
            ctx,
            "expected a numeric bytes-per-second rate",
        ));
    }
    let rate = whole_number(value)
        .ok_or_else(|| Exception::throw_type(ctx, "expected an exactly representable integer rate"))?;
    Ok(if rate > 0 {
        rate.unsigned_abs()
    } else {
        25_000
    })
}

fn local_ped(rt: &JsRuntime, ctx: &Ctx<'_>) -> rquickjs::Result<i64> {
    let host = rt.host(ctx)?;
    Ok(i64::from(client(ctx, &*host)?.local_ped().unwrap_or(0)))
}

pub(crate) fn client<'a>(
    ctx: &Ctx<'_>,
    host: &'a dyn Host,
) -> rquickjs::Result<&'a dyn ClientHost> {
    host.client()
        .ok_or_else(|| Exception::throw_message(ctx, "this function needs a client host"))
}
