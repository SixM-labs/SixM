//! `PerformHttpRequest`, which only a server has.

use std::rc::Weak;
use std::time::Duration;

use rquickjs::function::Opt;
use rquickjs::{Ctx, Exception, Filter, Function, Object, Value as JsValue};
use sixm_core::{FuncRef, HttpRequest};

use crate::natives::data_bytes;
use crate::runtime::JsRuntime;

/// Installs `PerformHttpRequest(url, callback, method, data, headers, options)`.
pub(crate) fn install<'js>(ctx: &Ctx<'js>, weak: &Weak<JsRuntime>) -> rquickjs::Result<()> {
    let weak = weak.clone();
    let perform = Function::new(
        ctx.clone(),
        move |ctx: Ctx<'js>,
              url: String,
              callback: Function<'js>,
              method: Opt<Option<String>>,
              data: Opt<Option<JsValue<'js>>>,
              headers: Opt<Option<Object<'js>>>,
              options: Opt<Option<Object<'js>>>|
              -> rquickjs::Result<()> {
            let rt = JsRuntime::upgrade(&ctx, &weak)?;
            let body = match data.0.flatten() {
                Some(val) => Some(data_bytes(&ctx, &val)?),
                None => None,
            };
            let request = HttpRequest {
                url,
                method: method.0.flatten().unwrap_or_else(|| "GET".to_owned()),
                headers: headers
                    .0
                    .flatten()
                    .as_ref()
                    .map(|obj| read_headers(&ctx, obj))
                    .transpose()?
                    .unwrap_or_default(),
                body,
                timeout: options
                    .0
                    .flatten()
                    .as_ref()
                    .map(|obj| read_timeout(&ctx, obj))
                    .transpose()?
                    .flatten(),
            };
            let reference: FuncRef = rt
                .pin_function(&ctx, callback)
                .map_err(|err| Exception::throw_message(&ctx, &err.to_string()))?;
            rt.host(&ctx)?
                .perform_http_request(&rt.resource, request, reference)
                .map_err(|err| Exception::throw_message(&ctx, &err.to_string()))
        },
    )?;
    ctx.globals().set("PerformHttpRequest", perform)
}

/// Reads an HTTP headers object. Both keys and values must be strings or numbers.
pub(crate) fn read_headers(
    ctx: &Ctx<'_>,
    obj: &Object<'_>,
) -> rquickjs::Result<Vec<(String, String)>> {
    let mut headers = Vec::new();
    for key in obj.own_keys::<String>(Filter::new().string().enum_only()) {
        let name = key?;
        let value: JsValue<'_> = obj.get(&name)?;
        headers.push((name, header_text(ctx, &value)?));
    }
    Ok(headers)
}

fn header_text(ctx: &Ctx<'_>, value: &JsValue<'_>) -> rquickjs::Result<String> {
    if let Some(text) = value.as_string() {
        return text.to_string();
    }
    if let Some(int) = value.as_int() {
        return Ok(int.to_string());
    }
    if let Some(float) = value.as_float() {
        return Ok(float.to_string());
    }
    Err(Exception::throw_type(
        ctx,
        &format!("HTTP headers must be strings, not {}", value.type_name()),
    ))
}

fn read_timeout(ctx: &Ctx<'_>, options: &Object<'_>) -> rquickjs::Result<Option<Duration>> {
    let val: JsValue<'_> = options.get("timeout")?;
    if val.is_undefined() || val.is_null() {
        return Ok(None);
    }
    let Some(millis) = val.as_number() else {
        return Err(Exception::throw_type(
            ctx,
            "the HTTP timeout must be a positive number of milliseconds",
        ));
    };
    if !millis.is_finite() || millis <= 0.0 {
        return Err(Exception::throw_range(
            ctx,
            "the HTTP timeout must be a positive number of milliseconds",
        ));
    }
    Ok(Some(Duration::from_secs_f64(millis / 1000.0)))
}
