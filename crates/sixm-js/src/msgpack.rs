//! Global `msgpack.pack(value)` and `msgpack.unpack(bytes)` helpers.

use std::rc::Weak;

use rquickjs::function::Opt;
use rquickjs::{Ctx, Exception, Function, Object, TypedArray, Value as JsValue};
use sixm_core::msgpack;

use crate::natives::data_bytes;
use crate::runtime::JsRuntime;

/// Installs the global `msgpack` object (`pack` and `unpack`).
pub(crate) fn install<'js>(ctx: &Ctx<'js>, weak: &Weak<JsRuntime>) -> rquickjs::Result<()> {
    let obj = Object::new(ctx.clone())?;

    {
        let weak = weak.clone();
        obj.set(
            "pack",
            Function::new(
                ctx.clone(),
                move |ctx: Ctx<'js>, value: Opt<JsValue<'js>>| -> rquickjs::Result<JsValue<'js>> {
                    let rt = JsRuntime::upgrade(&ctx, &weak)?;
                    let value = value
                        .0
                        .unwrap_or_else(|| JsValue::new_undefined(ctx.clone()));
                    let converted = rt
                        .to_network_value(&ctx, &value)
                        .map_err(|err| Exception::throw_message(&ctx, &err.to_string()))?;
                    let packed = msgpack::pack(&converted)
                        .map_err(|err| Exception::throw_message(&ctx, &err.to_string()))?;
                    TypedArray::<u8>::new_copy(ctx.clone(), &packed).map(TypedArray::into_value)
                },
            )?,
        )?;
    }

    {
        let weak = weak.clone();
        obj.set(
            "unpack",
            Function::new(
                ctx.clone(),
                move |ctx: Ctx<'js>, data: JsValue<'js>| -> rquickjs::Result<JsValue<'js>> {
                    let rt = JsRuntime::upgrade(&ctx, &weak)?;
                    let bytes = data_bytes(&ctx, &data)?;
                    let value = msgpack::unpack(&bytes)
                        .map_err(|err| Exception::throw_message(&ctx, &err.to_string()))?;
                    rt.to_js(&ctx, &value)
                        .map_err(|err| Exception::throw_message(&ctx, &err.to_string()))
                },
            )?,
        )?;
    }

    ctx.globals().set("msgpack", obj)?;
    Ok(())
}
