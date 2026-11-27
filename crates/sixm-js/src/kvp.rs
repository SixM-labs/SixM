//! Resource key-value store natives.

use std::collections::HashMap;
use std::rc::Weak;

use rquickjs::{Ctx, Exception, Function, IntoJs, Value as JsValue};

use crate::convert::bytes_to_js;
use crate::natives::data_bytes;
use crate::runtime::JsRuntime;

#[derive(Debug)]
struct Find {
    keys: Vec<String>,
    next: usize,
}

#[derive(Debug, Default)]
pub(crate) struct Finds {
    last_handle: i64,
    open: HashMap<i64, Find>,
}

pub(crate) fn install(ctx: &Ctx<'_>, weak: &Weak<JsRuntime>) -> rquickjs::Result<()> {
    install_getters(ctx, weak)?;
    install_setters(ctx, weak)?;
    install_find(ctx, weak)?;
    Ok(())
}

fn install_getters<'js>(ctx: &Ctx<'js>, weak: &Weak<JsRuntime>) -> rquickjs::Result<()> {
    let globals = ctx.globals();
    let get_string = {
        let weak = weak.clone();
        Function::new(
            ctx.clone(),
            move |ctx: Ctx<'js>, key: String| -> rquickjs::Result<JsValue<'js>> {
                let rt = JsRuntime::upgrade(&ctx, &weak)?;
                match rt.host(&ctx)?.kvp_get(rt.resource.as_ref(), &key) {
                    Some(bytes) => bytes_to_js(&ctx, &bytes)
                        .map_err(|err| Exception::throw_message(&ctx, &err.to_string())),
                    None => Ok(JsValue::new_null(ctx)),
                }
            },
        )?
    };
    let get_int = {
        let weak = weak.clone();
        Function::new(
            ctx.clone(),
            move |ctx: Ctx<'js>, key: String| -> rquickjs::Result<i64> {
                let rt = JsRuntime::upgrade(&ctx, &weak)?;
                Ok(stored_text(&rt, &ctx, &key)?
                    .and_then(|text| text.trim().parse::<i64>().ok())
                    .unwrap_or(0))
            },
        )?
    };
    let get_float = {
        let weak = weak.clone();
        Function::new(
            ctx.clone(),
            move |ctx: Ctx<'js>, key: String| -> rquickjs::Result<f64> {
                let rt = JsRuntime::upgrade(&ctx, &weak)?;
                Ok(stored_text(&rt, &ctx, &key)?
                    .and_then(|text| text.trim().parse::<f64>().ok())
                    .unwrap_or(0.0))
            },
        )?
    };
    for (name, func) in [
        ("GetResourceKvpString", get_string),
        ("GetResourceKvpInt", get_int),
        ("GetResourceKvpFloat", get_float),
    ] {
        globals.set(name, func)?;
    }
    Ok(())
}

fn install_setters<'js>(ctx: &Ctx<'js>, weak: &Weak<JsRuntime>) -> rquickjs::Result<()> {
    let globals = ctx.globals();
    let set_string = {
        let weak = weak.clone();
        Function::new(
            ctx.clone(),
            move |ctx: Ctx<'js>, key: String, value: JsValue<'js>| -> rquickjs::Result<bool> {
                let rt = JsRuntime::upgrade(&ctx, &weak)?;
                let bytes = data_bytes(&ctx, &value)?;
                Ok(rt.host(&ctx)?.kvp_set(rt.resource.as_ref(), &key, &bytes))
            },
        )?
    };
    let set_int = {
        let weak = weak.clone();
        Function::new(
            ctx.clone(),
            move |ctx: Ctx<'js>, key: String, value: i64| -> rquickjs::Result<bool> {
                let rt = JsRuntime::upgrade(&ctx, &weak)?;
                Ok(rt
                    .host(&ctx)?
                    .kvp_set(rt.resource.as_ref(), &key, value.to_string().as_bytes()))
            },
        )?
    };
    let set_float = {
        let weak = weak.clone();
        Function::new(
            ctx.clone(),
            move |ctx: Ctx<'js>, key: String, value: f64| -> rquickjs::Result<bool> {
                let rt = JsRuntime::upgrade(&ctx, &weak)?;
                Ok(rt
                    .host(&ctx)?
                    .kvp_set(rt.resource.as_ref(), &key, value.to_string().as_bytes()))
            },
        )?
    };
    let delete = {
        let weak = weak.clone();
        Function::new(
            ctx.clone(),
            move |ctx: Ctx<'js>, key: String| -> rquickjs::Result<bool> {
                let rt = JsRuntime::upgrade(&ctx, &weak)?;
                Ok(rt.host(&ctx)?.kvp_delete(rt.resource.as_ref(), &key))
            },
        )?
    };
    for (name, func) in [
        ("SetResourceKvp", set_string.clone()),
        ("SetResourceKvpNoSync", set_string),
        ("SetResourceKvpInt", set_int.clone()),
        ("SetResourceKvpIntNoSync", set_int),
        ("SetResourceKvpFloat", set_float.clone()),
        ("SetResourceKvpFloatNoSync", set_float),
        ("DeleteResourceKvp", delete.clone()),
        ("DeleteResourceKvpNoSync", delete),
    ] {
        globals.set(name, func)?;
    }
    Ok(())
}

fn install_find<'js>(ctx: &Ctx<'js>, weak: &Weak<JsRuntime>) -> rquickjs::Result<()> {
    let globals = ctx.globals();
    let start_find = {
        let weak = weak.clone();
        Function::new(
            ctx.clone(),
            move |ctx: Ctx<'js>, prefix: String| -> rquickjs::Result<i64> {
                let rt = JsRuntime::upgrade(&ctx, &weak)?;
                let keys = rt.host(&ctx)?.kvp_keys(rt.resource.as_ref(), &prefix);
                let mut finds = rt.kvp_finds.borrow_mut();
                finds.last_handle += 1;
                let handle = finds.last_handle;
                finds.open.insert(handle, Find { keys, next: 0 });
                Ok(handle)
            },
        )?
    };
    let find = {
        let weak = weak.clone();
        Function::new(
            ctx.clone(),
            move |ctx: Ctx<'js>, handle: i64| -> rquickjs::Result<JsValue<'js>> {
                let rt = JsRuntime::upgrade(&ctx, &weak)?;
                let mut finds = rt.kvp_finds.borrow_mut();
                let Some(cursor) = finds.open.get_mut(&handle) else {
                    return Ok(JsValue::new_null(ctx));
                };
                let key = cursor.keys.get(cursor.next).cloned();
                if key.is_some() {
                    cursor.next += 1;
                }
                match key {
                    Some(k) => k.into_js(&ctx),
                    None => Ok(JsValue::new_null(ctx)),
                }
            },
        )?
    };
    let end_find = {
        let weak = weak.clone();
        Function::new(
            ctx.clone(),
            move |ctx: Ctx<'js>, handle: i64| -> rquickjs::Result<()> {
                let rt = JsRuntime::upgrade(&ctx, &weak)?;
                rt.kvp_finds.borrow_mut().open.remove(&handle);
                Ok(())
            },
        )?
    };
    let flush = {
        let weak = weak.clone();
        Function::new(ctx.clone(), move |ctx: Ctx<'js>| -> rquickjs::Result<()> {
            let rt = JsRuntime::upgrade(&ctx, &weak)?;
            rt.host(&ctx)?.kvp_flush();
            Ok(())
        })?
    };
    for (name, func) in [
        ("StartFindKvp", start_find),
        ("FindKvp", find),
        ("EndFindKvp", end_find),
        ("FlushResourceKvp", flush),
    ] {
        globals.set(name, func)?;
    }
    Ok(())
}

fn stored_text(rt: &JsRuntime, ctx: &Ctx<'_>, key: &str) -> rquickjs::Result<Option<String>> {
    let bytes = rt.host(ctx)?.kvp_get(rt.resource.as_ref(), key);
    Ok(bytes.and_then(|bytes| String::from_utf8(bytes).ok()))
}
