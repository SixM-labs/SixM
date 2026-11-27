//! Evaluation of `fxmanifest.lua` in a sandboxed Lua state.
//!
//! A manifest is ordinary Lua. Any global that is not defined is a directive
//! recorder: calling it records its arguments under the directive's name. The
//! recorded entries are then sorted into the [`Manifest`] fields.

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use mlua::{Function, HookTriggers, Lua, LuaOptions, MultiValue, StdLib, Value, VmState};

use crate::error::ManifestError;

/// Memory a manifest may allocate.
const MEMORY_LIMIT: usize = 16 * 1024 * 1024;
/// Lua VM instructions a manifest may execute before it is aborted.
const INSTRUCTION_LIMIT: u64 = 10_000_000;
/// Upper bounds on the manifest copied out of the Lua heap.
const MAX_ENTRIES: usize = 8_192;
const MAX_ENTRY_BYTES: usize = 1024 * 1024;
/// The instruction hook runs once per this many instructions.
const HOOK_INTERVAL: u32 = 10_000;
/// Globals a manifest cannot reach. They read as nil instead of becoming
/// directive recorders. `load`, `dofile`, `loadfile`, `pcall` and `xpcall` are
/// also removed from the base library: an instruction-limit error must not be
/// catchable, or a manifest could loop forever around a `pcall`.
const BLOCKED_GLOBALS: [&str; 13] = [
    "coroutine",
    "debug",
    "dofile",
    "io",
    "load",
    "loadfile",
    "os",
    "package",
    "pcall",
    "require",
    "setmetatable",
    "utf8",
    "xpcall",
];
/// Directive names that take a list. Their entries are normalised to the
/// singular even when given a plain string.
const PLURAL_DIRECTIVES: [&str; 8] = [
    "dependencies",
    "exports",
    "files",
    "games",
    "server_exports",
    "server_scripts",
    "client_scripts",
    "shared_scripts",
];

/// Parsed contents of a `fxmanifest.lua`.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Manifest {
    /// Value of the last `fx_version` directive.
    pub fx_version: Option<String>,
    /// Games the resource supports, such as `gta5`.
    pub games: Vec<String>,
    /// Scripts loaded on the server, as written (globs allowed).
    pub server_scripts: Vec<String>,
    /// Scripts sent to clients, as written (globs allowed).
    pub client_scripts: Vec<String>,
    /// Scripts loaded on both sides, as written (globs allowed).
    pub shared_scripts: Vec<String>,
    /// Files that clients may download from the resource.
    pub files: Vec<String>,
    /// Resources that must start before this one. Entries starting with `/`
    /// are server or game constraints and are kept here unchanged.
    pub dependencies: Vec<String>,
    /// Exports registered by server scripts.
    pub server_exports: Vec<String>,
    /// Exports registered by client scripts.
    pub exports: Vec<String>,
    /// Whether the resource opts into Lua 5.4 (`lua54 'yes'`).
    pub lua54: bool,
    /// Every other directive in declaration order, e.g. `("author", "me")`.
    /// Chained arguments are stored under the directive name plus `_extra`.
    pub metadata: Vec<(String, String)>,
}

/// Evaluates a manifest and returns what it declares.
///
/// `chunk_name` is the Lua chunk name, for example `@my_resource/fxmanifest.lua`,
/// and prefixes line numbers in syntax and runtime errors. The manifest runs in
/// a fresh sandbox with a 16 MiB memory limit and an instruction budget.
pub fn parse_manifest(source: &str, chunk_name: &str) -> Result<Manifest, ManifestError> {
    let lua = Lua::new_with(
        StdLib::TABLE | StdLib::STRING | StdLib::MATH,
        LuaOptions::default(),
    )
    .map_err(lua_error)?;
    lua.set_memory_limit(MEMORY_LIMIT).map_err(lua_error)?;

    let tripped = Rc::new(Cell::new(false));
    install_instruction_limit(&lua, Rc::clone(&tripped)).map_err(lua_error)?;

    let sink = Rc::new(Sink::default());
    install_globals(&lua, &sink).map_err(lua_error)?;

    let outcome = lua.load(source).set_name(chunk_name).exec();
    if let Some(error) = sink.failure.take() {
        return Err(error);
    }
    if tripped.get() {
        return Err(ManifestError::InstructionLimit {
            limit: INSTRUCTION_LIMIT,
        });
    }
    outcome.map_err(lua_error)?;
    Ok(build(sink.entries.take()))
}

/// Entries recorded while the manifest runs, in declaration order.
#[derive(Default)]
struct Sink {
    /// `(key, value)` pairs, with the key already normalised.
    entries: RefCell<Vec<(String, String)>>,
    entry_bytes: Cell<usize>,
    /// The first invalid value. It stops the script and is reported instead of
    /// the Lua error it caused.
    failure: RefCell<Option<ManifestError>>,
}

/// Aborts the script after [`INSTRUCTION_LIMIT`] instructions. Once tripped,
/// every later hook call fails too, so nothing can continue past the limit.
fn install_instruction_limit(lua: &Lua, tripped: Rc<Cell<bool>>) -> mlua::Result<()> {
    let ticks = Cell::new(0_u64);
    let max_ticks = INSTRUCTION_LIMIT / u64::from(HOOK_INTERVAL);
    lua.set_hook(
        HookTriggers::new().every_nth_instruction(HOOK_INTERVAL),
        move |_, _| {
            let used = ticks.get() + 1;
            ticks.set(used);
            if used > max_ticks {
                tripped.set(true);
                return Err(mlua::Error::runtime("instruction limit reached"));
            }
            Ok(VmState::Continue)
        },
    )
}

/// Removes the blocked globals and makes every other unknown global a
/// directive recorder.
fn install_globals(lua: &Lua, sink: &Rc<Sink>) -> mlua::Result<()> {
    let globals = lua.globals();
    for name in BLOCKED_GLOBALS {
        globals.raw_set(name, Value::Nil)?;
    }

    let sink = Rc::clone(sink);
    let index = lua.create_function(
        move |lua, (_, key): (Value, Value)| -> mlua::Result<Option<Function>> {
            let Value::String(key) = key else {
                return Ok(None);
            };
            let name = key.to_string_lossy();
            if BLOCKED_GLOBALS.contains(&name.as_str()) {
                return Ok(None);
            }
            directive(lua, Rc::from(name.as_str()), Rc::clone(&sink)).map(Some)
        },
    )?;

    let metatable = lua.create_table()?;
    metatable.raw_set("__index", index)?;
    globals.set_metatable(Some(metatable))
}

/// Builds the recorder for directive `name`. Calling it records the argument
/// and returns the chained recorder, so `data_file 'A' 'B'` also works.
fn directive(lua: &Lua, name: Rc<str>, sink: Rc<Sink>) -> mlua::Result<Function> {
    lua.create_function(move |lua, args: MultiValue| {
        for arg in args {
            let key = directive_key(&name, matches!(arg, Value::Table(_)));
            record(&sink, &name, &key, &arg).map_err(|error| fail(&sink, error))?;
        }
        chained(lua, &name, &sink)
    })
}

/// Builds the function returned by a directive call. Its arguments are stored
/// as `<name>_extra`.
fn chained(lua: &Lua, name: &Rc<str>, sink: &Rc<Sink>) -> mlua::Result<Function> {
    let (name, sink) = (Rc::clone(name), Rc::clone(sink));
    lua.create_function(move |lua, args: MultiValue| {
        let key = format!("{name}_extra");
        for arg in args {
            record(&sink, &name, &key, &arg).map_err(|error| fail(&sink, error))?;
        }
        ignored(lua)
    })
}

/// A function that accepts anything and returns itself, used past the second
/// call of a chain so further chaining does not fail.
fn ignored(lua: &Lua) -> mlua::Result<Function> {
    lua.create_function(|lua, _: MultiValue| ignored(lua))
}

/// Stores `error` for the caller and returns a Lua error that stops the script.
fn fail(sink: &Sink, error: ManifestError) -> mlua::Error {
    let message = error.to_string();
    *sink.failure.borrow_mut() = Some(error);
    mlua::Error::runtime(message)
}

/// Records every element of `value` under `key`.
fn record(sink: &Sink, directive: &str, key: &str, value: &Value) -> Result<(), ManifestError> {
    let mut entries = sink.entries.borrow_mut();
    let mut push = |value: &Value| {
        let value = scalar(directive, value)?;
        let bytes = sink.entry_bytes.get() + key.len() + value.len();
        if entries.len() >= MAX_ENTRIES || bytes > MAX_ENTRY_BYTES {
            return Err(ManifestError::OutputLimit);
        }
        sink.entry_bytes.set(bytes);
        entries.push((key.to_owned(), value));
        Ok(())
    };
    match value {
        Value::Table(table) => {
            for element in table.sequence_values::<Value>() {
                push(&element.map_err(lua_error)?)?;
            }
        }
        other => push(other)?,
    }
    Ok(())
}

/// Text of a string or number value.
fn scalar(directive: &str, value: &Value) -> Result<String, ManifestError> {
    match value {
        Value::String(text) => Ok(text.to_string_lossy()),
        Value::Integer(number) => Ok(number.to_string()),
        Value::Number(number) => Ok(number.to_string()),
        other => Err(ManifestError::UnsupportedValue {
            directive: directive.to_owned(),
            kind: other.type_name(),
        }),
    }
}

/// Key under which a directive's value is stored.
///
/// Tables and known plural names are normalised to the singular: `dependencies`
/// becomes `dependency`, and any other trailing `s` is stripped. Other names
/// given a plain string keep their spelling.
fn directive_key(name: &str, is_table: bool) -> String {
    if !is_table && !PLURAL_DIRECTIVES.contains(&name) {
        return name.to_owned();
    }
    match name {
        "dependencies" => "dependency".to_owned(),
        _ => name.strip_suffix('s').unwrap_or(name).to_owned(),
    }
}

/// Sorts recorded entries into the manifest fields. Anything else becomes
/// metadata.
fn build(entries: Vec<(String, String)>) -> Manifest {
    let mut manifest = Manifest::default();
    for (key, value) in entries {
        match key.as_str() {
            "fx_version" => manifest.fx_version = Some(value),
            "game" => manifest.games.push(value),
            "server_script" => manifest.server_scripts.push(value),
            "client_script" => manifest.client_scripts.push(value),
            "shared_script" => manifest.shared_scripts.push(value),
            "file" => manifest.files.push(value),
            "dependency" => manifest.dependencies.push(value),
            "server_export" => manifest.server_exports.push(value),
            "export" => manifest.exports.push(value),
            "lua54" => manifest.lua54 = value == "yes",
            _ => manifest.metadata.push((key, value)),
        }
    }
    manifest
}

/// Converts a Lua error into a [`ManifestError`], keeping the Lua message
/// (which already includes the chunk name and line).
fn lua_error(error: mlua::Error) -> ManifestError {
    let message = match error {
        mlua::Error::SyntaxError { message, .. } | mlua::Error::RuntimeError(message) => message,
        other => other.to_string(),
    };
    ManifestError::Lua(message)
}

#[cfg(test)]
mod tests {
    use super::{Manifest, ManifestError, parse_manifest};

    fn parse(source: &str) -> Manifest {
        parse_manifest(source, "@test/fxmanifest.lua").expect("manifest should parse")
    }

    #[test]
    fn recorded_metadata_has_entry_and_byte_limits() {
        for source in [
            "for i = 1, 9000 do description 'small' end",
            "local text = string.rep('x', 1024); for i = 1, 1024 do description(text) end",
        ] {
            assert!(matches!(
                parse_manifest(source, "@test/fxmanifest.lua"),
                Err(ManifestError::OutputLimit)
            ));
        }
    }

    fn metadata_of(manifest: &Manifest, key: &str) -> Vec<String> {
        manifest
            .metadata
            .iter()
            .filter(|(k, _)| k == key)
            .map(|(_, v)| v.clone())
            .collect()
    }

    #[test]
    fn string_directive_is_one_entry() {
        let manifest = parse("author 'me'");
        assert_eq!(
            manifest.metadata,
            vec![("author".to_owned(), "me".to_owned())]
        );
    }

    #[test]
    fn table_directive_is_one_entry_per_element() {
        let manifest = parse("client_scripts { 'a.lua', 'b.lua' }");
        assert_eq!(manifest.client_scripts, ["a.lua", "b.lua"]);
    }

    #[test]
    fn numbers_keep_their_text_form() {
        let manifest = parse("version(1.2)\nrevision(5)\nbuild(2.0)");
        assert_eq!(metadata_of(&manifest, "version"), ["1.2"]);
        assert_eq!(metadata_of(&manifest, "revision"), ["5"]);
        assert_eq!(metadata_of(&manifest, "build"), ["2"]);
    }

    #[test]
    fn invalid_value_names_the_directive() {
        let error = parse_manifest("author(true)", "@t/fxmanifest.lua").unwrap_err();
        assert!(matches!(
            &error,
            ManifestError::UnsupportedValue { directive, kind: "boolean" } if directive == "author"
        ));
        assert_eq!(
            error.to_string(),
            "directive `author` cannot take a boolean value"
        );
    }

    #[test]
    fn nested_table_element_is_rejected() {
        let error = parse_manifest("files { { 'x' } }", "@t/fxmanifest.lua").unwrap_err();
        assert!(matches!(
            &error,
            ManifestError::UnsupportedValue { directive, kind: "table" } if directive == "files"
        ));
    }

    #[test]
    fn dependencies_table_normalises_to_dependency() {
        let manifest = parse("dependencies { 'a', 'b' }\ndependency 'c'");
        assert_eq!(manifest.dependencies, ["a", "b", "c"]);
    }

    #[test]
    fn plural_string_normalises_to_singular() {
        let manifest = parse("files 'x.json'\nserver_exports 'api'\ngames 'gta5'");
        assert_eq!(manifest.files, ["x.json"]);
        assert_eq!(manifest.server_exports, ["api"]);
        assert_eq!(manifest.games, ["gta5"]);
    }

    #[test]
    fn unknown_plural_string_keeps_its_name() {
        let manifest = parse("this_is 'x'");
        assert_eq!(metadata_of(&manifest, "this_is"), ["x"]);
    }

    #[test]
    fn unknown_table_key_strips_one_s_like_fivem() {
        let manifest = parse("this_is { 'x' }");
        assert_eq!(metadata_of(&manifest, "this_i"), ["x"]);
    }

    #[test]
    fn known_singulars_map_to_fields() {
        let manifest = parse(
            "fx_version 'cerulean'\nfx_version 'bodacious'\nlua54 'yes'\n\
             server_script 's.lua'\nshared_script 'sh.lua'\nclient_script 'c.lua'\n\
             export 'e'\nserver_export 'se'",
        );
        assert_eq!(manifest.fx_version.as_deref(), Some("bodacious"));
        assert!(manifest.lua54);
        assert_eq!(manifest.server_scripts, ["s.lua"]);
        assert_eq!(manifest.shared_scripts, ["sh.lua"]);
        assert_eq!(manifest.client_scripts, ["c.lua"]);
        assert_eq!(manifest.exports, ["e"]);
        assert_eq!(manifest.server_exports, ["se"]);
        assert_eq!(manifest.metadata, Vec::<(String, String)>::new());
    }

    #[test]
    fn lua54_is_true_only_for_yes() {
        assert!(!parse("lua54 'no'").lua54);
    }

    #[test]
    fn chained_argument_is_stored_as_extra() {
        let manifest = parse("data_file 'HANDLING_FILE' 'data/handling.meta'");
        assert_eq!(
            manifest.metadata,
            vec![
                ("data_file".to_owned(), "HANDLING_FILE".to_owned()),
                (
                    "data_file_extra".to_owned(),
                    "data/handling.meta".to_owned()
                ),
            ]
        );
    }

    #[test]
    fn chained_table_is_one_extra_per_element() {
        let manifest = parse("data_file 'X' { 'a.meta', 'b.meta' }");
        assert_eq!(
            metadata_of(&manifest, "data_file_extra"),
            ["a.meta", "b.meta"]
        );
    }

    #[test]
    fn calls_past_the_second_in_a_chain_are_ignored() {
        let manifest = parse("data_file 'A' 'B' 'C'");
        assert_eq!(metadata_of(&manifest, "data_file_extra"), ["B"]);
    }

    #[test]
    fn locals_loops_and_string_functions_work() {
        let manifest = parse(
            "local files = {}\nfor i = 1, 3 do files[#files + 1] = string.upper('a') .. i end\n\
             client_scripts(files)",
        );
        assert_eq!(manifest.client_scripts, ["A1", "A2", "A3"]);
    }

    #[test]
    fn syntax_error_names_chunk_and_line() {
        let error =
            parse_manifest("author 'a'\nclient_scripts {", "@res/fxmanifest.lua").unwrap_err();
        let message = error.to_string();
        assert!(message.contains("res/fxmanifest.lua:2:"), "{message}");
    }

    #[test]
    fn runtime_error_carries_lua_message() {
        let error = parse_manifest("local x = nil + 1", "@res/fxmanifest.lua").unwrap_err();
        assert!(error.to_string().contains("fxmanifest.lua:1:"), "{error}");
    }

    #[test]
    fn infinite_loop_is_aborted() {
        let error = parse_manifest("while true do end", "@t/fxmanifest.lua").unwrap_err();
        assert!(matches!(error, ManifestError::InstructionLimit { .. }));
    }

    #[test]
    fn memory_limit_aborts_huge_allocation() {
        let error =
            parse_manifest("local s = string.rep('x', 64 * 1024 * 1024)", "@t/f.lua").unwrap_err();
        assert!(matches!(error, ManifestError::Lua(_)));
    }

    #[test]
    fn dangerous_globals_read_as_nil() {
        let source = "assert(io == nil, 'io')\nassert(os == nil, 'os')\nassert(load == nil, 'load')\n\
                      assert(dofile == nil, 'dofile')\nassert(require == nil, 'require')\n\
                      assert(debug == nil, 'debug')\nassert(package == nil, 'package')\n\
                      assert(pcall == nil, 'pcall')";
        parse(source);
    }

    #[test]
    fn metadata_keeps_declaration_order() {
        let manifest = parse("author 'me'\ndescription 'x'\nauthor 'you'");
        let keys: Vec<&str> = manifest.metadata.iter().map(|(k, _)| k.as_str()).collect();
        assert_eq!(keys, ["author", "description", "author"]);
    }
}
