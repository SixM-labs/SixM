//! Construction of the sandboxed Lua state and the small Lua-side helpers.
//!
//! mlua refuses to load the real `debug` library in safe mode, so the two
//! functions scripts may use (`debug.traceback` and `debug.getinfo`) are
//! provided here on top of mlua's safe stack inspection.

use std::borrow::Cow;

use mlua::debug::Debug;
use mlua::{Function, Lua, LuaOptions, LuaString, MultiValue, StdLib, Table, Value as LuaValue};

/// Functions captured from the state before the sandbox trims it.
pub(crate) struct Helpers {
    /// The original `tostring`, used by `print`.
    pub(crate) tostring: Function,
    /// The original `type`. The vector module wraps it to name vectors.
    pub(crate) type_fn: Function,
    /// The `Wait` shim: yields its argument from a coroutine.
    pub(crate) wait: Function,
    /// Runs a task body under `xpcall`, so an error comes back as `false, traceback`.
    pub(crate) task_wrapper: Function,
}

/// Captures `coroutine.yield` and friends as upvalues, so script code cannot replace them.
const WAIT_SOURCE: &str = r#"
local yield, isyieldable, error = coroutine.yield, coroutine.isyieldable, error
return function(ms)
	if not isyieldable() then
		error("Wait can only be called from a thread (Citizen.CreateThread)", 2)
	end
	yield(ms)
end
"#;

/// Receives the traceback message handler as `...`, and captures `xpcall` as an upvalue.
const TASK_WRAPPER_SOURCE: &str = r"
local traceback = ...
local xpcall = xpcall
return function(task, ...)
	local ok, err = xpcall(task, traceback, ...)
	if not ok then
		return false, err
	end
end
";

/// Creates a Lua state that has only the permitted standard libraries.
pub(crate) fn new_state() -> mlua::Result<Lua> {
    let libs = StdLib::COROUTINE
        | StdLib::TABLE
        | StdLib::STRING
        | StdLib::UTF8
        | StdLib::MATH
        | StdLib::OS;
    Lua::new_with(libs, LuaOptions::default())
}

/// Removes the unsafe parts of a fresh state, installs `debug`, and returns the helpers the runtime needs.
pub(crate) fn prepare(lua: &Lua) -> mlua::Result<Helpers> {
    let globals = lua.globals();
    let tostring: Function = globals.raw_get("tostring")?;
    let type_fn: Function = globals.raw_get("type")?;
    let original_load: Function = globals.raw_get("load")?;

    let wait: Function = lua.load(WAIT_SOURCE).set_name("=sixm wait").eval()?;
    let handler = lua.create_function(|lua, message: LuaValue| {
        if let LuaValue::String(text) = &message {
            let text = text.to_string_lossy();
            return Ok(LuaValue::String(lua.traceback(Some(text.as_str()), 1)?));
        }
        Ok(message)
    })?;
    let task_wrapper: Function = lua
        .load(TASK_WRAPPER_SOURCE)
        .set_name("=sixm task")
        .call(handler)?;

    let text_load = lua.create_function(move |lua, args: MultiValue| {
        let mut args = args.into_vec();
        if args.len() < 3 {
            args.resize(3, LuaValue::Nil);
        }
        args[2] = LuaValue::String(lua.create_string("t")?);
        let results: MultiValue = original_load.call(MultiValue::from_vec(args))?;
        Ok(results)
    })?;

    globals.raw_set("dofile", LuaValue::Nil)?;
    globals.raw_set("loadfile", LuaValue::Nil)?;
    globals.raw_set("require", LuaValue::Nil)?;
    globals.raw_set("load", text_load)?;
    retain_fields(
        &globals.raw_get::<Table>("os")?,
        &["time", "clock", "date", "difftime"],
    )?;
    globals.raw_set("debug", debug_table(lua)?)?;

    Ok(Helpers {
        tostring,
        type_fn,
        wait,
        task_wrapper,
    })
}

/// Builds the `debug` table: `traceback` and `getinfo`, the only parts scripts may use.
fn debug_table(lua: &Lua) -> mlua::Result<Table> {
    let debug = lua.create_table()?;
    debug.raw_set(
        "traceback",
        lua.create_function(|lua, message: Option<LuaString>| {
            let message = message.map(|text| text.to_string_lossy());
            lua.traceback(message.as_deref(), 1)
        })?,
    )?;
    debug.raw_set(
        "getinfo",
        lua.create_function(|lua, level: usize| {
            let Some(frame) = lua.inspect_stack(level, FrameInfo::capture) else {
                return Ok(None);
            };
            frame.to_table(lua).map(Some)
        })?,
    )?;
    Ok(debug)
}

/// Stack-frame details that `debug.getinfo` reports.
struct FrameInfo {
    current_line: Option<usize>,
    source: Option<String>,
    short_src: Option<String>,
    what: &'static str,
    line_defined: Option<usize>,
    last_line_defined: Option<usize>,
    name: Option<String>,
    name_what: Option<&'static str>,
}

impl FrameInfo {
    fn capture(debug: &Debug<'_>) -> Self {
        let source = debug.source();
        let names = debug.names();
        Self {
            current_line: debug.current_line(),
            source: source.source.map(Cow::into_owned),
            short_src: source.short_src.map(Cow::into_owned),
            what: source.what,
            line_defined: source.line_defined,
            last_line_defined: source.last_line_defined,
            name: names.name.map(Cow::into_owned),
            name_what: names.name_what,
        }
    }

    fn to_table(&self, lua: &Lua) -> mlua::Result<Table> {
        let table = lua.create_table()?;
        table.raw_set(
            "currentline",
            self.current_line.and_then(|line| i64::try_from(line).ok()),
        )?;
        table.raw_set("source", self.source.as_deref())?;
        table.raw_set("short_src", self.short_src.as_deref())?;
        table.raw_set("what", self.what)?;
        table.raw_set(
            "linedefined",
            self.line_defined.and_then(|line| i64::try_from(line).ok()),
        )?;
        table.raw_set(
            "lastlinedefined",
            self.last_line_defined
                .and_then(|line| i64::try_from(line).ok()),
        )?;
        table.raw_set("name", self.name.as_deref())?;
        table.raw_set("namewhat", self.name_what.unwrap_or(""))?;
        Ok(table)
    }
}

/// Removes every field of `table` whose name is not in `keep`.
fn retain_fields(table: &Table, keep: &[&str]) -> mlua::Result<()> {
    let names = table
        .pairs::<String, LuaValue>()
        .map(|pair| pair.map(|(name, _)| name))
        .collect::<mlua::Result<Vec<_>>>()?;
    for name in names {
        if !keep.contains(&name.as_str()) {
            table.raw_set(name, LuaValue::Nil)?;
        }
    }
    Ok(())
}
