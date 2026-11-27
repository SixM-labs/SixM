mod common;

use std::rc::Rc;
use std::time::{Duration, Instant};

use common::{call, first, new_host, start, text, tick};
use sixm_core::{EventContext, EventSource, ScriptError, ScriptRuntime, Value};

#[test]
fn shutdown_drops_active_persistent_refs_and_cycles() {
    let host = new_host();
    let rt = start(
        &host,
        "life",
        &["ping"],
        &[(
            "index.js",
            r#"
            setTick(() => {});
            setTimeout(() => {}, 5000);
            setInterval(() => {}, 100);
            on("test:evt", () => {});
            AddStateBagChangeHandler("global", "k", () => {});
            (async () => { await Wait(1000); })();
            exports("ping", () => "pong");
            "#,
        )],
    );

    assert_eq!(text(&rt, "ping"), "pong");
    let weak = Rc::downgrade(&rt);
    host.stop(&rt);
    drop(rt);
    assert_eq!(weak.strong_count(), 0, "JsRuntime leaked via cycle or Persistent ref");
}

#[test]
fn drop_without_explicit_shutdown_is_clean() {
    let host = new_host();
    let rt = start(
        &host,
        "autodrop",
        &["ok"],
        &[(
            "index.js",
            r#"
            setTick(() => {});
            setTimeout(() => {}, 1000);
            exports("ok", () => true);
            "#,
        )],
    );
    assert_eq!(first(&rt, "ok"), Value::Bool(true));
    let weak = Rc::downgrade(&rt);
    // Remove host's Rc and drop our Rc without calling rt.shutdown() manually.
    host.stop(&rt);
    drop(rt);
    assert_eq!(weak.strong_count(), 0);
}

#[test]
fn post_shutdown_calls_are_safe() {
    let host = new_host();
    let rt = start(
        &host,
        "after",
        &["ping"],
        &[(
            "index.js",
            r#"
            on("a", () => {});
            exports("ping", () => 1);
            "#,
        )],
    );

    let debug_str = format!("{rt:?}");
    assert!(debug_str.contains("JsRuntime"));
    assert!(rt.handles_event("a"));

    rt.shutdown();
    assert!(!rt.handles_event("a"));

    let now = Instant::now() + Duration::from_secs(10);
    rt.tick(now);
    rt.run_timers(now);
    assert_eq!(rt.next_deadline(), None);
    let ctx = EventContext::default();
    assert!(matches!(
        rt.deliver_event("a", EventSource::Local, &[], &ctx),
        Err(ScriptError::ResourceNotRunning(_))
    ));

    let err = rt.call_export("ping", &[], None).unwrap_err();
    assert!(matches!(err, ScriptError::ResourceNotRunning(_)));

    tick(&host, now);
    let _ = call;
}
