//! Sandbox contents, `json`, and the host-facing globals.

mod common;

use common::{call, first, new_client_host, new_host, new_host_with_convars, start, text};
use sixm_core::{ConvarKind, ScriptError, ScriptRuntime, Value};

#[test]
fn unsafe_libraries_and_functions_are_removed() {
    let host = new_host();
    let rt = start(
        &host,
        "sealed",
        &["probe"],
        &[(
            "main.lua",
            "exports('probe', function()\n\
               return io == nil and require == nil and dofile == nil and loadfile == nil\n\
                 and package == nil and debug.sethook == nil and os.execute == nil\n\
                 and os.getenv == nil and os.exit == nil and os.remove == nil\n\
                 and type(os.time) == 'function' and type(debug.traceback) == 'function'\n\
             end)\n",
        )],
    );
    assert_eq!(first(&rt, "probe"), Value::Bool(true));
}

#[test]
fn load_accepts_text_and_rejects_bytecode() {
    let host = new_host();
    let rt = start(
        &host,
        "loader",
        &["text", "bytecode"],
        &[(
            "main.lua",
            "exports('text', function() return load('return 7')() end)\n\
             exports('bytecode', function()\n\
               local binary = string.dump(function() return 1 end)\n\
               local chunk, err = load(binary)\n\
               return chunk == nil and err ~= nil\n\
             end)\n",
        )],
    );
    assert_eq!(first(&rt, "text"), Value::Int(7));
    assert_eq!(first(&rt, "bytecode"), Value::Bool(true));
}

#[test]
fn allocation_and_execution_limits_leave_the_resource_usable() {
    let host = new_host();
    let rt = start(
        &host,
        "limits",
        &[],
        &[(
            "main.lua",
            r"
            local function calculate()
                local total = 0
                for i = 1, 4000000 do total = total + i end
                return total
            end
            exports('compute', function() return pcall(calculate) end)
            exports('coroutine', function() return coroutine.resume(coroutine.create(calculate)) end)
            exports('allocate', function() return string.rep('x', 70 * 1024 * 1024) end)
            exports('ok', function()
                local ok, result = xpcall(function() error('ordinary error') end,
                    function() return 7 end)
                local wrapped = coroutine.wrap(function() coroutine.yield(nil, result) end)
                local empty, value = wrapped()
                return not ok and empty == nil and value
            end)
            ",
        )],
    );
    for name in ["compute", "coroutine"] {
        let error = rt
            .call_export(name, &[], None)
            .expect_err("execution limit");
        assert!(error.to_string().contains("instruction limit"), "{error}");
    }
    let error = rt
        .call_export("allocate", &[], None)
        .expect_err("memory limit");
    assert!(error.to_string().contains("memory"), "{error}");
    assert_eq!(first(&rt, "ok"), Value::Int(7));
}

#[test]
fn json_encodes_arrays_maps_and_numbers() {
    let host = new_host();
    let rt = start(
        &host,
        "encode",
        &["enc"],
        &[(
            "main.lua",
            "exports('enc', function(v) return json.encode(v) end)\n",
        )],
    );
    let encode = |value: Value| {
        let out = call(&rt, "enc", &[value]);
        out.into_iter()
            .next()
            .and_then(|v| v.as_str().map(str::to_owned))
    };
    assert_eq!(
        encode(Value::Array(vec![Value::Int(1), Value::Int(2)])).as_deref(),
        Some("[1,2]")
    );
    assert_eq!(encode(Value::Array(Vec::new())).as_deref(), Some("[]"));
    assert_eq!(
        encode(Value::Map(vec![(Value::from("a"), Value::Int(1))])).as_deref(),
        Some(r#"{"a":1}"#)
    );
    assert_eq!(encode(Value::Float(1.5)).as_deref(), Some("1.5"));
    assert!(matches!(
        rt.call_export("enc", &[Value::Float(f64::NAN)], None),
        Err(ScriptError::Runtime(message)) if message.contains("NaN")
    ));
}

#[test]
fn json_decodes_and_keeps_integers() {
    let host = new_host();
    let rt = start(
        &host,
        "decode",
        &["dec", "number"],
        &[(
            "main.lua",
            "exports('dec', function(s) return json.decode(s) end)\n\
             exports('number', function() return math.type(json.decode('3')) end)\n",
        )],
    );
    let decoded = call(&rt, "dec", &[Value::from(r#"{"a":[1,2.5],"b":3}"#)]);
    let Value::Map(pairs) = &decoded[0] else {
        panic!("expected a map, got {decoded:?}");
    };
    assert!(pairs.contains(&(
        Value::from("a"),
        Value::Array(vec![Value::Int(1), Value::Float(2.5)])
    )));
    assert!(pairs.contains(&(Value::from("b"), Value::Int(3))));
    assert_eq!(text(&rt, "number"), "integer");
    assert!(rt.call_export("dec", &[Value::from("{")], None).is_err());
}

#[test]
fn resource_files_stay_inside_the_resource() {
    let host = new_host();
    let rt = start(
        &host,
        "files",
        &["escape", "save", "load", "absolute"],
        &[(
            "main.lua",
            "exports('escape', function() return LoadResourceFile(GetCurrentResourceName(), '../secret') end)\n\
             exports('absolute', function() return LoadResourceFile(GetCurrentResourceName(), '/etc/passwd') end)\n\
             exports('save', function() return SaveResourceFile(GetCurrentResourceName(), 'data/out.txt', 'hi') end)\n\
             exports('load', function() return LoadResourceFile(GetCurrentResourceName(), 'data/out.txt') end)\n",
        )],
    );
    assert_eq!(first(&rt, "escape"), Value::Nil);
    assert_eq!(first(&rt, "absolute"), Value::Nil);
    assert_eq!(first(&rt, "save"), Value::Bool(true));
    assert_eq!(first(&rt, "load"), Value::from("hi"));
}

#[test]
fn resource_files_accept_backslashes_and_refuse_other_resources() {
    let host = new_host();
    let rt = start(
        &host,
        "backslash",
        &["roundtrip", "foreign"],
        &[(
            "main.lua",
            "exports('roundtrip', function()\n\
               SaveResourceFile(GetCurrentResourceName(), 'data/out.txt', 'saved')\n\
               return LoadResourceFile(GetCurrentResourceName(), 'data\\\\out.txt')\n\
             end)\n\
             exports('foreign', function() return SaveResourceFile('other', 'x.txt', 'y') end)\n",
        )],
    );
    assert_eq!(first(&rt, "roundtrip"), Value::from("saved"));
    assert_eq!(first(&rt, "foreign"), Value::Bool(false));
}

#[test]
fn convars_commands_and_names_reach_the_host() {
    let host = new_host();
    let rt = start(
        &host,
        "natives",
        &["convars", "name"],
        &[(
            "main.lua",
            "ExecuteCommand('status')\n\
             exports('convars', function()\n\
               return { GetConvarInt('sv_maxclients', 1), GetConvarInt('missing', 7), GetConvar('missing', 'fallback') }\n\
             end)\n\
             exports('name', function() return GetCurrentResourceName() end)\n",
        )],
    );
    assert_eq!(
        first(&rt, "convars"),
        Value::Array(vec![Value::Int(32), Value::Int(7), Value::from("fallback")])
    );
    assert_eq!(first(&rt, "name"), Value::from("natives"));
    assert_eq!(host.commands.borrow().as_slice(), ["status"]);
}

#[test]
fn convar_setters_store_with_their_kind_and_only_the_server_has_them() {
    let host = new_host_with_convars(&[]);
    let rt = start(
        &host,
        "setters",
        &["probe"],
        &[(
            "main.lua",
            "SetConvar('plain', '1')\n\
             SetConvarReplicated('shared', 'race')\n\
             SetConvarServerInfo('info', 'x')\n\
             exports('probe', function() return type(SetConvar) end)\n",
        )],
    );
    assert_eq!(first(&rt, "probe"), Value::from("function"));
    assert_eq!(
        host.convar_sets.borrow().as_slice(),
        [
            ("plain".to_owned(), "1".to_owned(), ConvarKind::Plain),
            (
                "shared".to_owned(),
                "race".to_owned(),
                ConvarKind::Replicated
            ),
            ("info".to_owned(), "x".to_owned(), ConvarKind::ServerInfo),
        ]
    );

    let client = new_client_host(1);
    let rt = start(
        &client,
        "setters",
        &["probe"],
        &[(
            "main.lua",
            "exports('probe', function() return type(SetConvarReplicated) end)\n",
        )],
    );
    assert_eq!(first(&rt, "probe"), Value::from("nil"));
}

#[test]
fn get_convar_bool_reads_the_usual_spellings_and_falls_back_to_the_default() {
    let host = new_host_with_convars(&[
        ("yes_word", " YES "),
        ("off_word", "off"),
        ("one", "1"),
        ("zero", "0"),
        ("maybe", "maybe"),
    ]);
    let rt = start(
        &host,
        "bools",
        &["probe"],
        &[(
            "main.lua",
            "exports('probe', function()\n\
               return { GetConvarBool('yes_word', false), GetConvarBool('off_word', true),\n\
                 GetConvarBool('one', false), GetConvarBool('zero', true),\n\
                 GetConvarBool('maybe', true), GetConvarBool('missing', false) }\n\
             end)\n",
        )],
    );
    assert_eq!(
        first(&rt, "probe"),
        Value::Array(vec![
            Value::Bool(true),
            Value::Bool(false),
            Value::Bool(true),
            Value::Bool(false),
            Value::Bool(true),
            Value::Bool(false),
        ])
    );
}

#[test]
fn get_convar_float_parses_trimmed_finite_numbers_and_falls_back_to_default() {
    let host = new_host_with_convars(&[
        ("gravity", " 9.81 "),
        ("whole", "-12"),
        ("nan_val", "NaN"),
        ("inf_val", "inf"),
        ("neg_inf", "-infinity"),
        ("bad", "not-a-float"),
    ]);
    let rt = start(
        &host,
        "floats",
        &["probe"],
        &[(
            "main.lua",
            "exports('probe', function()\n\
               return {\n\
                 GetConvarFloat('gravity', 1.0),\n\
                 GetConvarFloat('whole', 1.0),\n\
                 GetConvarFloat('nan_val', 2.5),\n\
                 GetConvarFloat('inf_val', 3.5),\n\
                 GetConvarFloat('neg_inf', 4.5),\n\
                 GetConvarFloat('bad', 5.5),\n\
                 GetConvarFloat('missing', 6.5),\n\
               }\n\
             end)\n",
        )],
    );
    assert_eq!(
        first(&rt, "probe"),
        Value::Array(vec![
            Value::Float(9.81),
            Value::Float(-12.0),
            Value::Float(2.5),
            Value::Float(3.5),
            Value::Float(4.5),
            Value::Float(5.5),
            Value::Float(6.5),
        ])
    );
}

#[test]
fn debug_traceback_and_getinfo_are_available() {
    let host = new_host();
    let rt = start(
        &host,
        "debugging",
        &["trace", "line", "what"],
        &[(
            "main.lua",
            "exports('trace', function() return debug.traceback('here') end)\n\
             exports('line', function() return debug.getinfo(1).currentline end)\n\
             exports('what', function() return debug.getinfo(1, 'S').what end)\n\
             exports('gone', function() return debug.sethook end)\n",
        )],
    );
    let trace = text(&rt, "trace");
    assert!(trace.starts_with("here\nstack traceback:"), "{trace}");
    assert_eq!(first(&rt, "line"), Value::Int(2));
    assert_eq!(text(&rt, "what"), "Lua");
    assert_eq!(first(&rt, "gone"), Value::Nil);
}
