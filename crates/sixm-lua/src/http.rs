//! `PerformHttpRequest` and `PerformHttpRequestAwait`, which only a server has.

use std::rc::Weak;
use std::time::Duration;

use mlua::{Function, Lua, LuaString, Table, Value as LuaValue};
use sixm_core::{FuncRef, HttpRequest};

use crate::runtime::{LuaRuntime, bind_fn, lua_err};

/// `PerformHttpRequestAwait`: starts the request with a promise as its callback, then
/// awaits the promise. The chunk receives `perform`, `promise.new` and `Await` as `...`.
const AWAIT_REQUEST_SOURCE: &str = r"
local perform, new, await = ...
local isyieldable, error = coroutine.isyieldable, error
return function(url, method, data, headers, options)
	if not isyieldable() then
		error('PerformHttpRequestAwait can only be called from a thread (Citizen.CreateThread)', 2)
	end
	local p = new()
	perform(url, function(status, body, responseHeaders, errorData)
		p:resolve({ status, body, responseHeaders, errorData })
	end, method, data, headers, options)
	local r = await(p)
	return r[1], r[2], r[3], r[4]
end
";

/// Installs `PerformHttpRequest(url, callback, method, data, headers, options)` and
/// `PerformHttpRequestAwait(url, method, data, headers, options)`. The second one
/// needs the `promise` table and `Await`, so it runs after `promise::install`.
///
/// `callback` receives `(statusCode, body, headers, errorData)`. A request
/// that fails on the network has status `0` and its message as `errorData`.
pub(crate) fn install(lua: &Lua, weak: &Weak<LuaRuntime>, globals: &Table) -> mlua::Result<()> {
    let perform = bind_fn(
        lua,
        weak,
        |rt,
         (url, callback, method, data, headers, options): (
            String,
            Function,
            Option<String>,
            Option<LuaString>,
            Option<Table>,
            Option<Table>,
        )| {
            let request = HttpRequest {
                url,
                method: method.unwrap_or_else(|| "GET".to_owned()),
                headers: headers
                    .as_ref()
                    .map(read_headers)
                    .transpose()?
                    .unwrap_or_default(),
                body: data.map(|data| data.as_bytes().to_vec()),
                timeout: options.as_ref().map(read_timeout).transpose()?.flatten(),
            };
            let reference: FuncRef = rt.pin_function(callback).map_err(lua_err)?;
            rt.host()?
                .perform_http_request(&rt.resource, request, reference)
                .map_err(lua_err)
        },
    )?;
    let new: Function = globals.get::<Table>("promise")?.raw_get("new")?;
    let await_fn: Function = globals.raw_get("Await")?;
    let await_request = lua
        .load(AWAIT_REQUEST_SOURCE)
        .set_name("=sixm http await")
        .call::<Function>((perform.clone(), new, await_fn))?;
    globals.raw_set("PerformHttpRequest", perform)?;
    globals.raw_set("PerformHttpRequestAwait", await_request)
}

/// Reads a headers table. Both names and values must be strings or numbers.
pub(crate) fn read_headers(table: &Table) -> mlua::Result<Vec<(String, String)>> {
    let mut headers = Vec::new();
    for pair in table.pairs::<LuaValue, LuaValue>() {
        let (name, value) = pair?;
        headers.push((header_text(&name)?, header_text(&value)?));
    }
    Ok(headers)
}

fn header_text(value: &LuaValue) -> mlua::Result<String> {
    match value {
        LuaValue::String(text) => Ok(text.to_str()?.to_owned()),
        LuaValue::Integer(number) => Ok(number.to_string()),
        LuaValue::Number(number) => Ok(number.to_string()),
        other => Err(mlua::Error::runtime(format!(
            "HTTP headers must be strings, not {}",
            other.type_name()
        ))),
    }
}

/// Reads `options.timeout`, in milliseconds. Other keys are ignored.
fn read_timeout(options: &Table) -> mlua::Result<Option<Duration>> {
    let Some(millis) = options.get::<Option<f64>>("timeout")? else {
        return Ok(None);
    };
    if !millis.is_finite() || millis <= 0.0 {
        return Err(mlua::Error::runtime(
            "the HTTP timeout must be a positive number of milliseconds",
        ));
    }
    Ok(Some(Duration::from_secs_f64(millis / 1000.0)))
}
