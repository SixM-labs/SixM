//! Client-only globals: `emitNet` / `TriggerServerEvent`,
//! `TriggerLatentServerEvent`, `PlayerId`, `GetPlayerServerId`,
//! `PlayerPedId`, `GetPlayerPed`, and `LocalPlayer`.

use std::rc::Weak;

use rquickjs::function::Rest;
use rquickjs::{Ctx, Exception, Function, Object, Value as JsValue};
use sixm_core::{ClientHost, Host, NativeRegistry, player_bag};

use crate::convert::values_to_js_return;
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
    install_adapter_natives(ctx, weak)?;
    Ok(())
}

fn install_adapter_natives<'js>(ctx: &Ctx<'js>, weak: &Weak<JsRuntime>) -> rquickjs::Result<()> {
    let Some(rt) = weak.upgrade() else {
        return Ok(());
    };
    let Ok(host) = rt.host(ctx) else {
        return Ok(());
    };
    let Some(registry) = host.client().and_then(ClientHost::adapter_natives) else {
        return Ok(());
    };
    drop(host);

    let globals = ctx.globals();
    for (name, func) in registry.iter() {
        let weak = weak.clone();
        let func = func.clone();
        globals.set(
            name,
            Function::new(
                ctx.clone(),
                move |ctx: Ctx<'js>, args: Rest<JsValue<'js>>| -> rquickjs::Result<JsValue<'js>> {
                    let rt = JsRuntime::upgrade(&ctx, &weak)?;
                    let values = rt
                        .to_values(&ctx, &args.0)
                        .map_err(|err| Exception::throw_message(&ctx, &err.to_string()))?;
                    let out = func(&values)
                        .map_err(|err| Exception::throw_message(&ctx, &err.to_string()))?;
                    values_to_js_return(&rt, &ctx, &out)
                        .map_err(|err| Exception::throw_message(&ctx, &err.to_string()))
                },
            )?,
        )?;
    }

    let invoke = make_invoke_native(ctx, weak, registry)?;
    globals.set("InvokeNative", invoke.clone())?;
    if let Ok(citizen) = globals.get::<_, Object<'js>>("Citizen") {
        citizen.set("InvokeNative", invoke)?;
    }
    Ok(())
}

fn make_invoke_native<'js>(
    ctx: &Ctx<'js>,
    weak: &Weak<JsRuntime>,
    registry: NativeRegistry,
) -> rquickjs::Result<Function<'js>> {
    let weak = weak.clone();
    Function::new(
        ctx.clone(),
        move |ctx: Ctx<'js>,
              hash: JsValue<'js>,
              args: Rest<JsValue<'js>>|
              -> rquickjs::Result<JsValue<'js>> {
            let rt = JsRuntime::upgrade(&ctx, &weak)?;
            let hash = native_hash(&ctx, &hash)?;
            let native = registry.get_by_hash(hash).ok_or_else(|| {
                Exception::throw_message(&ctx, &format!("unknown native hash {hash:#010x}"))
            })?;
            let values = rt
                .to_values(&ctx, &args.0)
                .map_err(|err| Exception::throw_message(&ctx, &err.to_string()))?;
            let out =
                native(&values).map_err(|err| Exception::throw_message(&ctx, &err.to_string()))?;
            values_to_js_return(&rt, &ctx, &out)
                .map_err(|err| Exception::throw_message(&ctx, &err.to_string()))
        },
    )
}

#[expect(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    reason = "native hashes are 32-bit values and may arrive signed via GetHashKey"
)]
fn native_hash(ctx: &Ctx<'_>, value: &JsValue<'_>) -> rquickjs::Result<u32> {
    let number =
        whole_number(value).filter(|&n| (i64::from(i32::MIN)..=i64::from(u32::MAX)).contains(&n));
    number.map(|n| n as u32).ok_or_else(|| {
        Exception::throw_type(ctx, "InvokeNative: native hash must be a 32-bit integer")
    })
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
    let rate = whole_number(value).ok_or_else(|| {
        Exception::throw_type(ctx, "expected an exactly representable integer rate")
    })?;
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
