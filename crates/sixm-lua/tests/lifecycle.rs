//! Shutdown and memory ownership.

mod common;

use std::rc::Rc;

use common::{new_host, start};
use sixm_core::{EventSource, Host, ScriptError, ScriptRuntime, Value};
use sixm_lua::LuaRuntime;

#[test]
fn shutdown_drops_all_state_and_the_last_rc_frees_the_runtime() {
    let host = new_host();
    let rt = start(
        &host,
        "lifecycle",
        &["ping"],
        &[(
            "main.lua",
            "exports('ping', function() return 1 end)\n\
             AddEventHandler('evt', function() end)\n\
             CreateThread(function() Wait(1000) end)\n",
        )],
    );
    let weak: std::rc::Weak<LuaRuntime> = Rc::downgrade(&rt);
    assert!(rt.handles_event("evt"));
    assert!(rt.next_deadline().is_none());

    host.stop(&rt);

    assert!(matches!(
        rt.call_export("ping", &[], None),
        Err(ScriptError::ResourceNotRunning(_))
    ));
    assert!(matches!(
        rt.call_ref(1, &[]),
        Err(ScriptError::ResourceNotRunning(_))
    ));
    assert!(!rt.handles_event("evt"));
    assert!(rt.next_deadline().is_none());
    assert!(
        host.trigger_event("evt", EventSource::Local, &[Value::Nil], None)
            .is_ok()
    );

    drop(rt);
    assert!(weak.upgrade().is_none(), "the runtime should be freed");
}

#[test]
fn register_command_accepts_a_missing_restricted_flag() {
    let host = new_host();
    let _rt = start(
        &host,
        "commands",
        &[],
        &[("main.lua", "RegisterCommand('hello', function() end)\n")],
    );
}

#[test]
fn memory_bytes_and_handled_events_report_activity() {
    let host = new_host();
    let rt = start(
        &host,
        "resmon_lua",
        &[],
        &[(
            "main.lua",
            "local t = {}; for i = 1, 100 do t[i] = string.rep('x', 32) end\n\
             AddEventHandler('ping', function() end)\n\
             AddEventHandler('ping', function() end)\n",
        )],
    );
    assert!(rt.memory_bytes() > 0);
    assert_eq!(rt.handled_events(), 0);
    host.trigger_event("ping", EventSource::Local, &[], None)
        .expect("trigger ping");
    assert_eq!(rt.handled_events(), 2);
}
