//! Scheduling: `Wait`, threads, timers and error isolation.

mod common;

use std::time::{Duration, Instant};

use common::{call, first, new_host, start, text, tick};
use sixm_core::{ScriptError, ScriptRuntime, Value};

/// Lua snippet that defines an ordered `log` and exports it as `log`.
const LOG_PRELUDE: &str = "local log = {}\n\
    exports('log', function() return table.concat(log, ',') end)\n";

#[test]
fn wait_timers_fire_in_deadline_order() {
    let host = new_host();
    let base = Instant::now();
    let rt = start(
        &host,
        "timers",
        &["log"],
        &[(
            "main.lua",
            &format!(
                "{LOG_PRELUDE}\
                 CreateThreadNow(function() Wait(1200) log[#log + 1] = 'c' end)\n\
                 CreateThreadNow(function() Wait(400) log[#log + 1] = 'a' end)\n\
                 CreateThreadNow(function() Wait(800) log[#log + 1] = 'b' end)\n"
            ),
        )],
    );

    tick(&host, base + Duration::from_millis(600));
    assert_eq!(text(&rt, "log"), "a");
    tick(&host, base + Duration::from_millis(1000));
    assert_eq!(text(&rt, "log"), "a,b");
    tick(&host, base + Duration::from_millis(1500));
    assert_eq!(text(&rt, "log"), "a,b,c");
}

#[test]
fn next_deadline_reports_the_earliest_timer() {
    let host = new_host();
    let rt = start(
        &host,
        "deadline",
        &[],
        &[(
            "main.lua",
            "CreateThreadNow(function() Wait(5000) end)\n\
             CreateThreadNow(function() Wait(2000) end)\n",
        )],
    );
    let deadline = rt.next_deadline().expect("two timers are pending");
    let earliest_in = deadline.saturating_duration_since(Instant::now());
    assert!(earliest_in <= Duration::from_millis(2000));
    assert!(earliest_in > Duration::from_millis(1000));
}

#[test]
fn wait_zero_resumes_on_each_following_frame() {
    let host = new_host();
    let base = Instant::now();
    let rt = start(
        &host,
        "frames",
        &["n"],
        &[(
            "main.lua",
            "local n = 0\n\
             exports('n', function() return n end)\n\
             CreateThreadNow(function() while true do Wait(0) n = n + 1 end end)\n",
        )],
    );
    assert_eq!(first(&rt, "n"), Value::Int(0));
    tick(&host, base);
    assert_eq!(first(&rt, "n"), Value::Int(1));
    tick(&host, base);
    assert_eq!(first(&rt, "n"), Value::Int(2));
    assert!(
        rt.next_deadline().is_none(),
        "Wait(0) must not create a timer"
    );
}

#[test]
fn create_thread_starts_next_frame_and_create_thread_now_runs_immediately() {
    let host = new_host();
    let base = Instant::now();
    let rt = start(
        &host,
        "starts",
        &["log"],
        &[(
            "main.lua",
            &format!(
                "{LOG_PRELUDE}\
                 Citizen.CreateThread(function() log[#log + 1] = 'thread' end)\n\
                 CreateThreadNow(function() log[#log + 1] = 'now' end)\n\
                 log[#log + 1] = 'main'\n"
            ),
        )],
    );
    assert_eq!(text(&rt, "log"), "now,main");
    tick(&host, base);
    assert_eq!(text(&rt, "log"), "now,main,thread");
}

#[test]
fn set_timeout_fires_once_and_clear_timeout_cancels() {
    let host = new_host();
    let base = Instant::now();
    let rt = start(
        &host,
        "timeouts",
        &["log", "kind"],
        &[(
            "main.lua",
            &format!(
                "{LOG_PRELUDE}\
                 local cancelled = SetTimeout(100, function() log[#log + 1] = 'cancelled' end)\n\
                 ClearTimeout(cancelled)\n\
                 SetTimeout(200, function() log[#log + 1] = 'kept' end)\n\
                 exports('kind', function() return math.type(cancelled) end)\n"
            ),
        )],
    );
    assert_eq!(first(&rt, "kind"), Value::from("integer"));
    tick(&host, base + Duration::from_millis(1000));
    assert_eq!(text(&rt, "log"), "kept");
    tick(&host, base + Duration::from_millis(2000));
    assert_eq!(text(&rt, "log"), "kept");
}

#[test]
fn wait_outside_a_thread_is_an_error() {
    let host = new_host();
    let rt = start(
        &host,
        "outside",
        &["bad"],
        &[("main.lua", "exports('bad', function() Wait(10) end)\n")],
    );
    match rt.call_export("bad", &[], None) {
        Err(ScriptError::Runtime(message)) => {
            assert!(
                message.contains("Wait can only be called from a thread"),
                "{message}"
            );
        }
        other => panic!("expected a runtime error, got {other:?}"),
    }
}

#[test]
fn an_erroring_task_does_not_stop_the_others() {
    let host = new_host();
    let base = Instant::now();
    let rt = start(
        &host,
        "isolation",
        &["log"],
        &[(
            "main.lua",
            &format!(
                "{LOG_PRELUDE}\
                 CreateThreadNow(function() error('boom') end)\n\
                 CreateThreadNow(function() Wait(10) log[#log + 1] = 'survived' end)\n"
            ),
        )],
    );
    tick(&host, base + Duration::from_millis(1000));
    assert_eq!(text(&rt, "log"), "survived");
}

#[test]
fn a_finished_task_can_be_reused_for_a_new_thread() {
    let host = new_host();
    let base = Instant::now();
    let rt = start(
        &host,
        "reuse",
        &["log"],
        &[(
            "main.lua",
            &format!(
                "{LOG_PRELUDE}\
                 for i = 1, 200 do\n\
                   CreateThreadNow(function() log[#log + 1] = tostring(i) end)\n\
                 end\n"
            ),
        )],
    );
    tick(&host, base);
    assert_eq!(call(&rt, "log", &[]).len(), 1);
    assert_eq!(text(&rt, "log").split(',').count(), 200);
}

#[test]
fn run_timers_leaves_next_frame_tasks_for_the_next_frame() {
    let host = new_host();
    let rt = start(
        &host,
        "frames",
        &["frames"],
        &[(
            "main.lua",
            "local frames = 0\n\
             CreateThread(function() while true do frames = frames + 1 Wait(0) end end)\n\
             exports('frames', function() return frames end)\n",
        )],
    );
    let now = Instant::now();
    rt.tick(now);
    rt.run_timers(now + Duration::from_millis(1));
    rt.run_timers(now + Duration::from_millis(2));
    assert_eq!(first(&rt, "frames"), Value::Int(1));
    rt.tick(now + Duration::from_millis(50));
    assert_eq!(first(&rt, "frames"), Value::Int(2));
}
