mod common;

use std::time::{Duration, Instant};

use common::{call, first, new_host, start, text};
use sixm_core::{EventContext, EventSource, ResourceName, ScriptRuntime, Value};

#[test]
fn set_tick_and_clear_tick_run_each_frame_until_cleared() {
    let host = new_host();
    let rt = start(
        &host,
        "ticks",
        &["count", "stop"],
        &[(
            "index.js",
            r#"
            let n = 0;
            const id = setTick(() => { n += 1; });
            exports("count", () => n);
            exports("stop", () => { clearTick(id); });
            "#,
        )],
    );

    let now = Instant::now();
    rt.tick(now);
    rt.tick(now + Duration::from_millis(50));
    assert_eq!(first(&rt, "count"), Value::Int(2));

    call(&rt, "stop", &[]);
    rt.tick(now + Duration::from_millis(100));
    assert_eq!(first(&rt, "count"), Value::Int(2));
}

#[test]
fn set_timeout_set_interval_and_fivem_aliases_work_with_deadlines() {
    let host = new_host();
    let rt = start(
        &host,
        "timers",
        &["log", "clearInt"],
        &[(
            "index.js",
            r#"
            const events = [];
            const t1 = setTimeout((a, b) => events.push("t:" + a + ":" + b), 100, "x", 7);
            const t2 = setTimeout(() => events.push("cancelled"), 50);
            clearTimeout(t2);
            SetTimeout(150, () => events.push("fivem"));
            const iv = setInterval((k) => events.push("iv:" + k), 60, "tick");

            exports("clearInt", () => { clearInterval(iv); clearTimeout(t1); });
            exports("log", () => events.join(","));
            "#,
        )],
    );

    let t0 = Instant::now();
    // Run a tick so scheduler tracks the base instant accurately
    rt.tick(t0);
    assert!(rt.next_deadline().is_some());

    rt.run_timers(t0 + Duration::from_millis(70));
    assert_eq!(text(&rt, "log"), "iv:tick");

    rt.run_timers(t0 + Duration::from_millis(130));
    assert_eq!(text(&rt, "log"), "iv:tick,t:x:7,iv:tick");

    rt.run_timers(t0 + Duration::from_millis(160));
    assert_eq!(text(&rt, "log"), "iv:tick,t:x:7,iv:tick,fivem");

    call(&rt, "clearInt", &[]);
    rt.run_timers(t0 + Duration::from_millis(500));
    assert_eq!(text(&rt, "log"), "iv:tick,t:x:7,iv:tick,fivem");
    assert_eq!(rt.next_deadline(), None);
}

#[test]
fn wait_and_delay_promises_preserve_source_and_invoker() {
    let host = new_host();
    let rt = start(
        &host,
        "waits",
        &["run", "result"],
        &[(
            "index.js",
            r#"
            let out = "";
            on("trigger", async (tag) => {
                const s0 = source;
                const inv0 = GetInvokingResource();
                await Wait(0);
                const s1 = source;
                const inv1 = GetInvokingResource();
                await Delay(40);
                const s2 = source;
                const inv2 = GetInvokingResource();
                out = `${tag}:${s0}/${s1}/${s2}:${inv0}/${inv1}/${inv2}`;
            });
            exports("result", () => out);
            "#,
        )],
    );

    let caller = ResourceName::from("caller");
    let ctx = EventContext::default();
    rt.deliver_event(
        "trigger",
        EventSource::Player(9),
        &[Value::from("ok")],
        &ctx,
    )
    .expect("deliver");
    let _ = caller;

    let t0 = Instant::now();
    // Wait(0) resolves on next frame (`tick`), Delay(40) resolves after deadline passes.
    rt.tick(t0);
    assert_eq!(text(&rt, "result"), "");

    rt.run_timers(t0 + Duration::from_millis(60));
    assert_eq!(text(&rt, "result"), "ok:9/9/9:null/null/null");
}

#[test]
fn unhandled_promise_rejections_do_not_crash_runtime() {
    let host = new_host();
    let rt = start(
        &host,
        "rej",
        &["ok"],
        &[(
            "index.js",
            r#"
            Promise.reject(new Error("unhandled boom"));
            exports("ok", () => "still fine");
            "#,
        )],
    );

    let now = Instant::now();
    rt.tick(now);
    rt.tick(now + Duration::from_millis(50));
    assert_eq!(text(&rt, "ok"), "still fine");
}
