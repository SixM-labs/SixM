//! Events: argument conversion, cancellation, network safety and `source`.

mod common;

use std::time::{Duration, Instant};

use common::{first, new_host, start, tick};
use sixm_core::{EventContext, EventSource, Host, ScriptRuntime, Value};

#[test]
fn event_arguments_round_trip_through_a_handler() {
    let host = new_host();
    let rt = start(
        &host,
        "echo",
        &["echo", "got"],
        &[(
            "main.lua",
            "local got\n\
             AddEventHandler('evt', function(...) got = { ... } end)\n\
             exports('got', function() return got end)\n\
             exports('echo', function(...) return ... end)\n",
        )],
    );

    let args = vec![
        Value::Int(1),
        Value::Float(2.0),
        Value::String(Box::from(&[0xff_u8, b'A'][..])),
        Value::Array(vec![Value::Int(1), Value::Array(Vec::new())]),
        Value::Map(vec![(Value::from("k"), Value::Float(1.5))]),
    ];

    let echoed = rt.call_export("echo", &args, None).expect("echo");
    assert_eq!(echoed, args);

    host.trigger_event("evt", EventSource::Local, &args[..2], None)
        .expect("trigger");
    assert_eq!(
        first(&rt, "got"),
        Value::Array(vec![Value::Int(1), Value::Float(2.0)])
    );
}

#[test]
fn cancel_event_is_visible_to_later_handlers_in_other_resources() {
    let host = new_host();
    let _first = start(
        &host,
        "canceller",
        &[],
        &[(
            "main.lua",
            "AddEventHandler('ping', function() CancelEvent() end)\n",
        )],
    );
    let watcher = start(
        &host,
        "watcher",
        &["seen"],
        &[(
            "main.lua",
            "local seen\n\
             AddEventHandler('ping', function() seen = WasEventCanceled() end)\n\
             exports('seen', function() return seen end)\n",
        )],
    );

    let canceled = host
        .trigger_event("ping", EventSource::Local, &[], None)
        .expect("trigger");
    assert!(canceled);
    assert_eq!(first(&watcher, "seen"), Value::Bool(true));
}

#[test]
fn network_events_need_register_net_event() {
    let host = new_host();
    let rt = start(
        &host,
        "net",
        &["hit", "who"],
        &[(
            "main.lua",
            "local hit, who\n\
             AddEventHandler('plain', function() hit = true end)\n\
             RegisterNetEvent('netok', function() who = source end)\n\
             exports('hit', function() return hit end)\n\
             exports('who', function() return who end)\n",
        )],
    );
    let ctx = EventContext::new();

    rt.deliver_event("plain", EventSource::Net(5), &[], &ctx)
        .expect("deliver");
    assert_eq!(first(&rt, "hit"), Value::Nil);

    rt.deliver_event("netok", EventSource::Net(5), &[], &ctx)
        .expect("deliver");
    assert_eq!(first(&rt, "who"), Value::Int(5));
}

#[test]
fn source_is_restored_after_a_wait() {
    let host = new_host();
    let base = Instant::now();
    let rt = start(
        &host,
        "sources",
        &["out"],
        &[(
            "main.lua",
            "local out\n\
             RegisterNetEvent('slow', function()\n\
               Wait(50)\n\
               out = source\n\
             end)\n\
             AddEventHandler('quick', function() local _ = source end)\n\
             exports('out', function() return out end)\n",
        )],
    );
    let ctx = EventContext::new();
    rt.deliver_event("slow", EventSource::Net(7), &[], &ctx)
        .expect("deliver slow");
    rt.deliver_event("quick", EventSource::Local, &[], &ctx)
        .expect("deliver quick");

    tick(&host, base + Duration::from_millis(500));
    assert_eq!(first(&rt, "out"), Value::Int(7));
}

#[test]
fn trigger_event_inside_a_handler_restores_source() {
    let host = new_host();
    let rt = start(
        &host,
        "nested",
        &["after", "inner"],
        &[(
            "main.lua",
            "local after, inner\n\
             RegisterNetEvent('outer', function()\n\
               TriggerEvent('inner')\n\
               after = source\n\
             end)\n\
             AddEventHandler('inner', function() inner = source end)\n\
             exports('after', function() return after end)\n\
             exports('inner', function() return inner end)\n",
        )],
    );
    let ctx = EventContext::new();
    rt.deliver_event("outer", EventSource::Net(9), &[], &ctx)
        .expect("deliver");
    assert_eq!(first(&rt, "after"), Value::Int(9));
    assert_eq!(first(&rt, "inner"), Value::from(""));
}
