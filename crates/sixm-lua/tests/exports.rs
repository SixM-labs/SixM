//! Exports, cross-resource calls and function references.

mod common;

use std::time::Instant;

use common::{first, new_host, start, tick};

use sixm_core::{Host, ScriptError, ScriptRuntime, Value};

#[test]
fn colon_and_dot_syntax_both_reach_an_export() {
    let host = new_host();
    let _provider = start(
        &host,
        "provider",
        &["add"],
        &[(
            "main.lua",
            "exports('add', function(a, b) return a + b end)\n",
        )],
    );
    let consumer = start(
        &host,
        "consumer",
        &["results"],
        &[(
            "main.lua",
            "local colon = exports.provider:add(1, 2)\n\
             local dot = exports.provider.add(1, 2)\n\
             local ok, err = pcall(function() return exports.provider.missing(1) end)\n\
             exports('results', function() return { colon, dot, ok, err } end)\n",
        )],
    );

    let results = first(&consumer, "results");
    let Value::Array(items) = results else {
        panic!("expected an array, got {results:?}");
    };
    assert_eq!(items[0], Value::Int(3));
    assert_eq!(items[1], Value::Int(3));
    assert_eq!(items[2], Value::Bool(false));
    let message = items[3].as_str().expect("error message");
    assert!(message.contains("has no export `missing`"), "{message}");
}

#[test]
fn a_missing_export_is_no_such_export() {
    let host = new_host();
    let rt = start(&host, "lonely", &[], &[("main.lua", "")]);
    assert!(matches!(
        rt.call_export("nothing", &[], None),
        Err(ScriptError::NoSuchExport { .. })
    ));
}

#[test]
fn a_declared_global_function_is_callable_as_an_export() {
    let host = new_host();
    let rt = start(
        &host,
        "declared",
        &["double"],
        &[("main.lua", "function double(x) return x * 2 end\n")],
    );
    assert_eq!(
        rt.call_export("double", &[Value::Int(21)], None)
            .expect("double"),
        vec![Value::Int(42)]
    );
}

#[test]
fn a_callback_passed_across_resources_is_called_and_released() {
    let host = new_host();
    let provider = start(
        &host,
        "holder",
        &[],
        &[(
            "main.lua",
            "local saved\n\
             exports('keep', function(cb) saved = cb end)\n\
             exports('fire', function(x) return saved(x) end)\n\
             exports('drop', function() saved = nil collectgarbage() collectgarbage() end)\n",
        )],
    );
    let _owner = start(
        &host,
        "owner",
        &[],
        &[(
            "main.lua",
            "exports.holder.keep(function(x) return x * 2 end)\n",
        )],
    );

    assert_eq!(
        provider
            .call_export("fire", &[Value::Int(21)], None)
            .expect("fire"),
        vec![Value::Int(42)]
    );

    assert!(host.released.borrow().is_empty());
    provider.call_export("drop", &[], None).expect("drop");
    let released = host.released.borrow().clone();
    assert_eq!(
        released.len(),
        1,
        "the handle should release once: {released:?}"
    );
    assert_eq!(released[0].owner.as_ref(), "owner");

    // Unretained references are freed at the owner's next frame.
    tick(&host, Instant::now());
    let owner = host
        .call_export("owner", "missing", &[], None)
        .expect_err("owner has no exports");
    assert!(matches!(owner, ScriptError::NoSuchExport { .. }));
    assert!(matches!(
        host.call_ref(&released[0], &[]),
        Err(ScriptError::StaleRef(_))
    ));
}

#[test]
fn values_nested_too_deep_are_rejected() {
    let host = new_host();
    let rt = start(
        &host,
        "deep",
        &["deep"],
        &[(
            "main.lua",
            "exports('deep', function()\n\
               local root = {}\n\
               local cur = root\n\
               for i = 1, 70 do cur.x = {} cur = cur.x end\n\
               return root\n\
             end)\n",
        )],
    );
    assert!(matches!(
        rt.call_export("deep", &[], None),
        Err(ScriptError::TooDeep(64))
    ));
}

#[test]
fn threads_cannot_cross_resources() {
    let host = new_host();
    let rt = start(
        &host,
        "threads",
        &["co"],
        &[(
            "main.lua",
            "exports('co', function() return coroutine.create(function() end) end)\n",
        )],
    );
    match rt.call_export("co", &[], None) {
        Err(ScriptError::Runtime(message)) => {
            assert!(message.contains("cannot pass a thread value"), "{message}");
        }
        other => panic!("expected a runtime error, got {other:?}"),
    }
}

#[test]
fn a_callback_held_by_two_resources_survives_one_release() {
    let host = new_host();
    let holder = "local saved\n\
                  exports('keep', function(cb) saved = cb end)\n\
                  exports('fire', function(x) return saved(x) end)\n\
                  exports('drop', function() saved = nil collectgarbage() collectgarbage() end)\n";
    let first_holder = start(&host, "first", &[], &[("main.lua", holder)]);
    let second_holder = start(&host, "second", &[], &[("main.lua", holder)]);
    let _owner = start(
        &host,
        "owner",
        &[],
        &[(
            "main.lua",
            "local cb = function(x) return x + 1 end\n\
             exports.first.keep(cb)\n\
             exports.second.keep(cb)\n",
        )],
    );

    first_holder.call_export("drop", &[], None).expect("drop");
    tick(&host, Instant::now());
    assert_eq!(
        second_holder
            .call_export("fire", &[Value::Int(1)], None)
            .expect("the second holder still owns a retain"),
        vec![Value::Int(2)]
    );

    second_holder.call_export("drop", &[], None).expect("drop");
    tick(&host, Instant::now());
    let released = host.released.borrow().clone();
    assert_eq!(released.len(), 2);
    assert!(matches!(
        host.call_ref(&released[1], &[]),
        Err(ScriptError::StaleRef(_))
    ));
}

#[test]
fn an_unkept_callback_is_freed_at_the_owners_next_frame() {
    let host = new_host();
    let sink = start(
        &host,
        "sink",
        &[],
        &[(
            "main.lua",
            "exports('ignore', function(cb) end)\n\
             exports('gc', function() collectgarbage() collectgarbage() end)\n",
        )],
    );
    let owner = start(
        &host,
        "owner",
        &["send"],
        &[(
            "main.lua",
            "function send() exports.sink.ignore(function() end) end\n",
        )],
    );
    owner.call_export("send", &[], None).expect("send");
    sink.call_export("gc", &[], None).expect("gc");
    let sent = sixm_core::FuncRef {
        owner: owner.resource().clone(),
        instance: owner.instance(),
        id: 1,
    };
    assert!(
        host.call_ref(&sent, &[]).is_ok(),
        "valid until the owner's next frame"
    );
    tick(&host, Instant::now());
    assert!(matches!(
        host.call_ref(&sent, &[]),
        Err(ScriptError::StaleRef(_))
    ));
}
