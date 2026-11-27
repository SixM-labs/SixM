//! Construction of the sandboxed Lua state and the small Lua-side helpers.
//!
//! mlua refuses to load the real `debug` library in safe mode, so the two
//! functions scripts may use (`debug.traceback` and `debug.getinfo`) are
//! provided here on top of mlua's safe stack inspection.

use std::borrow::Cow;
use std::cell::Cell;
use std::rc::Rc;

use mlua::debug::Debug;
use mlua::{
    Function, HookTriggers, Lua, LuaOptions, LuaString, MultiValue, StdLib, Table,
    Value as LuaValue, VmState,
};

const MEMORY_LIMIT: usize = 64 * 1024 * 1024;
const HOOK_INTERVAL: u32 = 1_000;
const INSTRUCTION_LIMIT: u32 = 2_000_000;

/// Shared by all coroutines in one resource, including nested host calls.
#[derive(Clone, Default)]
pub(crate) struct Budget(Rc<Cell<Option<u32>>>);

impl Budget {
    pub(crate) fn run<T>(&self, run: impl FnOnce() -> mlua::Result<T>) -> mlua::Result<T> {
        self.check()?;
        if self.0.get().is_some() {
            return run();
        }
        self.0.set(Some(INSTRUCTION_LIMIT / HOOK_INTERVAL));
        let _reset = ResetBudget(&self.0);
        let result = run();
        self.check()?;
        result
    }

    fn check(&self) -> mlua::Result<()> {
        if self.0.get() == Some(0) {
            return Err(mlua::Error::runtime("Lua instruction limit reached"));
        }
        Ok(())
    }

    fn step(&self) -> mlua::Result<VmState> {
        if let Some(remaining) = self.0.get() {
            self.0.set(Some(remaining.saturating_sub(1)));
        }
        self.check()?;
        Ok(VmState::Continue)
    }
}

struct ResetBudget<'a>(&'a Cell<Option<u32>>);

impl Drop for ResetBudget<'_> {
    fn drop(&mut self) {
        self.0.set(None);
    }
}

/// Keep protected calls yieldable and propagate a spent budget to the host.
const PROTECTED_SOURCE: &str = r"
local check = ...
local protect, protect_with = pcall, xpcall
local pack, unpack = table.pack, table.unpack
local create, resume, close = coroutine.create, coroutine.resume, coroutine.close
local assign_metatable, rawget, error, type = setmetatable, rawget, error, type

local function checked(...)
    check()
    return ...
end

function pcall(...)
    return checked(protect(...))
end

function xpcall(fn, handler, ...)
    if type(handler) ~= 'function' then return protect_with(fn, handler, ...) end
    return checked(protect_with(fn, function(err)
        check()
        return handler(err)
    end, ...))
end

function coroutine.resume(thread, ...)
    return checked(resume(thread, ...))
end

function coroutine.close(thread)
    return checked(close(thread))
end

function coroutine.wrap(fn)
    local thread = create(fn)
    return function(...)
        local results = pack(checked(resume(thread, ...)))
        if not results[1] then
            local _, failure = checked(close(thread))
            error(failure or results[2], 2)
        end
        return unpack(results, 2, results.n)
    end
end

function setmetatable(value, meta)
    if meta ~= nil and rawget(meta, '__gc') ~= nil then
        error('table finalizers are disabled', 2)
    end
    return assign_metatable(value, meta)
end
";

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
pub(crate) fn new_state(budget: &Budget) -> mlua::Result<Lua> {
    let libs = StdLib::COROUTINE
        | StdLib::TABLE
        | StdLib::STRING
        | StdLib::UTF8
        | StdLib::MATH
        | StdLib::OS;
    let lua = Lua::new_with(libs, LuaOptions::default())?;
    lua.set_memory_limit(MEMORY_LIMIT)?;
    let hook_budget = budget.clone();
    lua.set_global_hook(
        HookTriggers::new().every_nth_instruction(HOOK_INTERVAL),
        move |_, _| hook_budget.step(),
    )?;
    Ok(lua)
}

/// Removes the unsafe parts of a fresh state, installs `debug`, and returns the helpers the runtime needs.
pub(crate) fn prepare(lua: &Lua, budget: Budget) -> mlua::Result<Helpers> {
    let globals = lua.globals();
    let tostring: Function = globals.raw_get("tostring")?;
    let type_fn: Function = globals.raw_get("type")?;
    let original_load: Function = globals.raw_get("load")?;
    globals.raw_get::<Table>("coroutine")?.raw_set(
        "create",
        lua.create_function(|lua, function: Function| lua.create_thread(function))?,
    )?;
    let check = lua.create_function(move |_, ()| budget.check())?;
    lua.load(PROTECTED_SOURCE)
        .set_name("=sixm sandbox")
        .call::<()>(check)?;

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
        if let Some(LuaValue::String(chunk)) = args.first() {
            let bytes = chunk.as_bytes();
            let processed = crate::preprocess::lua_backticks(&bytes);
            if matches!(processed, Cow::Owned(_)) {
                args[0] = LuaValue::String(lua.create_string(processed.as_ref())?);
            }
        }
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
