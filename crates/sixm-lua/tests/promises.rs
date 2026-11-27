//! Promises: settling, reactions, `Await`, `promise.all` and `promise.first`,
//! and what happens to them at shutdown and across exports.

mod common;

use std::time::{Duration, Instant};

use common::{call, new_host, start, text, tick};
use sixm_core::{ScriptRuntime, Value};

/// Lua snippet that defines an ordered `log` and exports it as `log`.
const LOG_PRELUDE: &str = "local log = {}\n\
    exports('log', function() return table.concat(log, ',') end)\n";

#[test]
fn await_returns_the_value_of_a_fulfilled_promise() {
    let host = new_host();
    let rt = start(
        &host,
        "ready",
        &["log"],
        &[(
            "main.lua",
            &format!(
                "{LOG_PRELUDE}\
                 local p = promise.new()\n\
                 p:resolve(42)\n\
                 CreateThreadNow(function() log[#log + 1] = tostring(Await(p)) end)\n"
            ),
        )],
    );
    assert_eq!(text(&rt, "log"), "42");
}

#[test]
fn await_parks_until_another_task_resolves_the_promise() {
    let host = new_host();
    let base = Instant::now();
    let rt = start(
        &host,
        "later",
        &["log"],
        &[(
            "main.lua",
            &format!(
                "{LOG_PRELUDE}\
                 local p = promise.new()\n\
                 CreateThreadNow(function()\n\
                     local value = Await(p)\n\
                     log[#log + 1] = 'got ' .. tostring(value)\n\
                 end)\n\
                 CreateThreadNow(function()\n\
                     Wait(100)\n\
                     log[#log + 1] = 'resolving'\n\
                     p:resolve('v')\n\
                 end)\n"
            ),
        )],
    );
    tick(&host, base + Duration::from_millis(50));
    assert_eq!(text(&rt, "log"), "");
    tick(&host, base + Duration::from_millis(200));
    assert_eq!(text(&rt, "log"), "resolving");
    tick(&host, base + Duration::from_millis(200));
    assert_eq!(text(&rt, "log"), "resolving,got v");
}

#[test]
fn a_rejection_raises_in_await_and_pcall_catches_it() {
    let host = new_host();
    let rt = start(
        &host,
        "rejected",
        &["log"],
        &[(
            "main.lua",
            &format!(
                "{LOG_PRELUDE}\
                 local p = promise.new()\n\
                 p:reject('nope')\n\
                 CreateThreadNow(function()\n\
                     local ok, err = pcall(Await, p)\n\
                     log[#log + 1] = tostring(ok) .. ':' .. tostring(err)\n\
                 end)\n"
            ),
        )],
    );
    assert_eq!(text(&rt, "log"), "false:nope");
}

#[test]
fn only_the_first_settle_counts() {
    let host = new_host();
    let rt = start(
        &host,
        "settle_once",
        &["log"],
        &[(
            "main.lua",
            &format!(
                "{LOG_PRELUDE}\
                 local p = promise.new()\n\
                 p:resolve(1)\n\
                 p:resolve(2)\n\
                 p:reject('late')\n\
                 CreateThreadNow(function() log[#log + 1] = tostring(Await(p)) end)\n"
            ),
        )],
    );
    assert_eq!(text(&rt, "log"), "1");
}

#[test]
fn next_chains_pass_values_along() {
    let host = new_host();
    let base = Instant::now();
    let rt = start(
        &host,
        "chain",
        &["log"],
        &[(
            "main.lua",
            &format!(
                "{LOG_PRELUDE}\
                 local p = promise.new()\n\
                 p:next(function(v) return v + 1 end)\n\
                    :next(function(v) return v * 10 end)\n\
                    :next(function(v) log[#log + 1] = tostring(v) end)\n\
                 p:resolve(4)\n"
            ),
        )],
    );
    for _ in 0..3 {
        tick(&host, base);
    }
    assert_eq!(text(&rt, "log"), "50");
}

#[test]
fn next_callbacks_run_on_a_later_frame_not_inside_resolve() {
    let host = new_host();
    let base = Instant::now();
    let rt = start(
        &host,
        "later_frame",
        &["log"],
        &[(
            "main.lua",
            &format!(
                "{LOG_PRELUDE}\
                 local p = promise.new()\n\
                 p:next(function(v) log[#log + 1] = 'callback ' .. v end)\n\
                 log[#log + 1] = 'before'\n\
                 p:resolve(1)\n\
                 log[#log + 1] = 'after'\n"
            ),
        )],
    );
    assert_eq!(text(&rt, "log"), "before,after");
    tick(&host, base);
    assert_eq!(text(&rt, "log"), "before,after,callback 1");
}

#[test]
fn an_error_in_a_callback_rejects_the_chained_promise() {
    let host = new_host();
    let base = Instant::now();
    let rt = start(
        &host,
        "callback_error",
        &["log"],
        &[(
            "main.lua",
            &format!(
                "{LOG_PRELUDE}\
                 local p = promise.new()\n\
                 p:next(function() error('boom', 0) end)\n\
                    :next(nil, function(reason) log[#log + 1] = 'caught ' .. reason end)\n\
                 p:resolve(1)\n"
            ),
        )],
    );
    for _ in 0..3 {
        tick(&host, base);
    }
    assert_eq!(text(&rt, "log"), "caught boom");
}

#[test]
fn a_callback_may_await_and_the_chain_resumes_after_it() {
    let host = new_host();
    let base = Instant::now();
    let rt = start(
        &host,
        "callback_await",
        &["log", "finish"],
        &[(
            "main.lua",
            &format!(
                "{LOG_PRELUDE}\
                 local b = promise.new()\n\
                 exports('finish', function() b:resolve(10) end)\n\
                 local p = promise.new()\n\
                 p:next(function(v) return Await(b) + v end)\n\
                    :next(function(v) log[#log + 1] = 'result ' .. v end)\n\
                 p:resolve(1)\n"
            ),
        )],
    );
    tick(&host, base);
    assert_eq!(text(&rt, "log"), "");
    call(&rt, "finish", &[]);
    for _ in 0..2 {
        tick(&host, base);
    }
    assert_eq!(text(&rt, "log"), "result 11");
}

#[test]
fn a_callback_returning_a_promise_adopts_its_eventual_value() {
    let host = new_host();
    let base = Instant::now();
    let rt = start(
        &host,
        "adopt",
        &["log", "finish"],
        &[(
            "main.lua",
            &format!(
                "{LOG_PRELUDE}\
                 local inner = promise.new()\n\
                 exports('finish', function() inner:resolve('in') end)\n\
                 local p = promise.new()\n\
                 p:next(function() return inner end)\n\
                    :next(function(v) log[#log + 1] = 'outer ' .. v end)\n\
                 p:resolve(1)\n"
            ),
        )],
    );
    tick(&host, base);
    tick(&host, base);
    assert_eq!(text(&rt, "log"), "");
    call(&rt, "finish", &[]);
    tick(&host, base);
    tick(&host, base);
    assert_eq!(text(&rt, "log"), "outer in");
}

#[test]
fn promise_all_fulfils_with_the_values_in_order() {
    let host = new_host();
    let base = Instant::now();
    let rt = start(
        &host,
        "all",
        &["log"],
        &[(
            "main.lua",
            &format!(
                "{LOG_PRELUDE}\
                 local a, b = promise.new(), promise.new()\n\
                 promise.all({{ a, b, 3 }}):next(function(list)\n\
                     log[#log + 1] = table.concat(list, '+')\n\
                 end)\n\
                 b:resolve(2)\n\
                 a:resolve(1)\n"
            ),
        )],
    );
    tick(&host, base);
    assert_eq!(text(&rt, "log"), "1+2+3");
}

#[test]
fn promise_all_rejects_with_the_first_rejection() {
    let host = new_host();
    let base = Instant::now();
    let rt = start(
        &host,
        "all_rejects",
        &["log"],
        &[(
            "main.lua",
            &format!(
                "{LOG_PRELUDE}\
                 local a, b = promise.new(), promise.new()\n\
                 promise.all({{ a, b }}):next(nil, function(reason)\n\
                     log[#log + 1] = 'rejected ' .. reason\n\
                 end)\n\
                 a:reject('bad')\n\
                 b:reject('worse')\n"
            ),
        )],
    );
    tick(&host, base);
    assert_eq!(text(&rt, "log"), "rejected bad");
}

#[test]
fn promise_first_settles_like_the_first_input() {
    let host = new_host();
    let base = Instant::now();
    let rt = start(
        &host,
        "first",
        &["log"],
        &[(
            "main.lua",
            &format!(
                "{LOG_PRELUDE}\
                 local a, b = promise.new(), promise.new()\n\
                 promise.first({{ a, b }}):next(function(v)\n\
                     log[#log + 1] = 'first ' .. v\n\
                 end)\n\
                 b:resolve('B')\n\
                 a:resolve('A')\n"
            ),
        )],
    );
    tick(&host, base);
    assert_eq!(text(&rt, "log"), "first B");
}

#[test]
fn promise_all_of_nothing_fulfils_at_once_with_an_empty_table() {
    let host = new_host();
    let rt = start(
        &host,
        "empty_all",
        &["log"],
        &[(
            "main.lua",
            &format!(
                "{LOG_PRELUDE}\
                 CreateThreadNow(function()\n\
                     log[#log + 1] = tostring(#Await(promise.all({{}})))\n\
                 end)\n"
            ),
        )],
    );
    assert_eq!(text(&rt, "log"), "0");
}

#[test]
fn await_outside_a_task_raises_an_error() {
    let host = new_host();
    let rt = start(
        &host,
        "outside",
        &["try"],
        &[(
            "main.lua",
            "exports('try', function() return Await(promise.new()) end)\n",
        )],
    );
    let err = rt
        .call_export("try", &[], None)
        .expect_err("Await outside a task must fail");
    assert!(
        err.to_string()
            .contains("Await can only be called from a thread"),
        "unexpected error: {err}"
    );
}

#[test]
fn an_export_callback_can_resolve_a_promise_the_caller_awaits() {
    let host = new_host();
    let base = Instant::now();
    let _other = start(
        &host,
        "other",
        &["get"],
        &[(
            "main.lua",
            "exports('get', function(callback)\n\
                 CreateThread(function()\n\
                     Wait(50)\n\
                     callback(7)\n\
                 end)\n\
             end)\n",
        )],
    );
    let waiter = start(
        &host,
        "waiter",
        &["log"],
        &[(
            "main.lua",
            &format!(
                "{LOG_PRELUDE}\
                 local p = promise.new()\n\
                 exports.other:get(function(v) p:resolve(v) end)\n\
                 CreateThreadNow(function() log[#log + 1] = 'got ' .. tostring(Await(p)) end)\n"
            ),
        )],
    );
    tick(&host, base + Duration::from_millis(10));
    assert_eq!(text(&waiter, "log"), "");
    tick(&host, base + Duration::from_millis(100));
    tick(&host, base + Duration::from_millis(100));
    assert_eq!(text(&waiter, "log"), "got 7");
}

#[test]
fn a_parked_task_stays_parked_across_many_ticks() {
    let host = new_host();
    let base = Instant::now();
    let rt = start(
        &host,
        "parked",
        &["log", "finish"],
        &[(
            "main.lua",
            &format!(
                "{LOG_PRELUDE}\
                 local p = promise.new()\n\
                 exports('finish', function(v) p:resolve(v) end)\n\
                 CreateThreadNow(function() log[#log + 1] = 'got ' .. tostring(Await(p)) end)\n"
            ),
        )],
    );
    for frame in 0..1000 {
        tick(&host, base + Duration::from_millis(frame));
    }
    assert_eq!(rt.next_deadline(), None);
    assert_eq!(text(&rt, "log"), "");
    call(&rt, "finish", &[Value::Int(5)]);
    tick(&host, base);
    assert_eq!(text(&rt, "log"), "got 5");
}

#[test]
fn an_unhandled_rejection_does_not_stop_the_runtime() {
    let host = new_host();
    let base = Instant::now();
    let rt = start(
        &host,
        "unhandled",
        &["log"],
        &[(
            "main.lua",
            &format!(
                "{LOG_PRELUDE}\
                 promise.new():reject('ignored')\n\
                 CreateThread(function() log[#log + 1] = 'still running' end)\n"
            ),
        )],
    );
    tick(&host, base);
    tick(&host, base);
    assert_eq!(text(&rt, "log"), "still running");
}

#[test]
fn shutdown_with_parked_tasks_and_pending_callbacks_is_clean() {
    let host = new_host();
    let rt = start(
        &host,
        "shutdown",
        &[],
        &[(
            "main.lua",
            "local p = promise.new()\n\
             CreateThreadNow(function() Await(p) end)\n\
             local done = promise.new()\n\
             done:next(function() print('never') end)\n\
             done:resolve(1)\n\
             CreateThreadNow(function() Wait(1000) end)\n",
        )],
    );
    host.stop(&rt);
    assert_eq!(rt.next_deadline(), None);
}

#[test]
fn a_promise_cannot_cross_an_export() {
    let host = new_host();
    let _source = start(
        &host,
        "source",
        &["take"],
        &[("main.lua", "exports('take', function(p) return 1 end)\n")],
    );
    let sender = start(
        &host,
        "sender",
        &["send"],
        &[(
            "main.lua",
            "exports('send', function() return exports.source:take(promise.new()) end)\n",
        )],
    );
    let err = sender
        .call_export("send", &[], None)
        .expect_err("a promise must not cross an export");
    assert!(
        err.to_string().contains("cannot pass a userdata value"),
        "unexpected error: {err}"
    );
}
