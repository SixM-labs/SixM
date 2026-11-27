mod common;

use std::time::Instant;

use common::{call, first, new_host, start, text, tick};
use sixm_core::{ScriptRuntime, Value};

#[test]
fn dangerous_globals_are_absent_or_disabled() {
    let host = new_host();
    let rt = start(
        &host,
        "sandbox",
        &["check"],
        &[(
            "index.js",
            r#"
            exports("check", () => {
                const absent = [
                    "require", "process", "fetch", "XMLHttpRequest",
                    "WebAssembly", "SharedArrayBuffer", "Atomics",
                    "eval", "Intl", "std", "os", "Deno", "Bun"
                ];
                for (const name of absent) {
                    if (typeof globalThis[name] !== "undefined") {
                        throw new Error(name + " should be undefined");
                    }
                }
                let threw = false;
                try { new Function("return 1")(); } catch (e) { threw = true; }
                if (!threw) throw new Error("new Function should throw");

                threw = false;
                try { (() => {}).constructor("return 1")(); } catch (e) { threw = true; }
                if (!threw) throw new Error("Function.prototype.constructor should throw");

                const forms = [async () => 1, function* () { yield 1; }, async function* () { yield 1; }];
                for (const form of forms) {
                    if (form.constructor !== Function) {
                        throw new Error("every function prototype should use the disabled constructor");
                    }
                }

                // Safe builtins still work
                const d = new Date(0);
                const j = JSON.parse(JSON.stringify({ a: 1, b: [true, "ok"] }));
                const m = new Map([["k", 7]]);
                const p = new Proxy({ x: 3 }, {});
                print("hello", 42);
                console.log("log");
                console.warn("warn");
                console.error("err");
                console.debug("dbg");
                return [d.getUTCFullYear(), j.a, j.b[1], m.get("k"), p.x];
            });
            "#,
        )],
    );

    assert_eq!(
        call(&rt, "check", &[]),
        vec![Value::Array(vec![
            Value::Int(1970),
            Value::Int(1),
            Value::from("ok"),
            Value::Int(7),
            Value::Int(3),
        ])]
    );
}

#[test]
fn infinite_loop_is_interrupted_and_uncatchable() {
    let host = new_host();
    let rt = start(
        &host,
        "spin",
        &["spin"],
        &[(
            "index.js",
            r#"
        exports("spin", () => {
            try {
                while (true) {}
            } catch (e) {
                return "escaped";
            }
            return "done";
        });
    "#,
        )],
    );

    let err = rt.call_export("spin", &[], None).unwrap_err();
    let msg = err.to_string();
    assert!(
        msg.contains("interrupted"),
        "expected interrupt error, got: {msg}"
    );

    // Runtime remains usable afterwards for normal calls
    let rt2 = start(
        &host,
        "ok",
        &["hi"],
        &[("index.js", r#"exports("hi", () => "ok")"#)],
    );
    assert_eq!(text(&rt2, "hi"), "ok");
    tick(&host, Instant::now());
}

#[test]
fn memory_and_stack_limits_throw_cleanly_without_aborting() {
    let host = new_host();
    let rt = start(
        &host,
        "limits",
        &["oom", "stack", "ok"],
        &[(
            "index.js",
            r#"
            exports("oom", () => {
                const a = [];
                while (true) {
                    a.push(new Array(100000).fill(1));
                }
            });
            exports("stack", () => {
                function rec() { return rec() + 1; }
                return rec();
            });
            exports("ok", () => 123);
            "#,
        )],
    );

    let oom_err = rt.call_export("oom", &[], None).unwrap_err().to_string();
    assert!(
        oom_err.to_lowercase().contains("memory") || oom_err.to_lowercase().contains("interrupted"),
        "unexpected oom error: {oom_err}"
    );

    let stack_err = rt.call_export("stack", &[], None).unwrap_err().to_string();
    assert!(
        stack_err.to_lowercase().contains("stack"),
        "unexpected stack error: {stack_err}"
    );

    assert_eq!(first(&rt, "ok"), Value::Int(123));
}

#[test]
fn redos_regex_is_interrupted_by_budget() {
    let host = new_host();
    let rt = start(
        &host,
        "redos",
        &["evil"],
        &[(
            "index.js",
            r#"
            exports("evil", () => {
                const re = /^(a+)+$/;
                return re.test("aaaaaaaaaaaaaaaaaaaaaaaaaaaa!");
            });
            "#,
        )],
    );

    let err = rt.call_export("evil", &[], None).unwrap_err().to_string();
    assert!(
        err.contains("interrupted"),
        "expected ReDoS interrupt, got: {err}"
    );
}
