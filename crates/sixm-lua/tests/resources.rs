//! Resource natives: `GetInvokingResource` across events, exports and tasks, and
//! the resource state, list and metadata functions read from the host.

mod common;

use std::time::{Duration, Instant};

use common::{call, first, new_client_host, new_host, start, tick};
use sixm_core::{EventSource, Host, ResourceState, Value};

/// A resource that fires `thing:happened` when its `fire` export is called.
const EMITTER: &str = "exports('fire', function() TriggerEvent('thing:happened') end)\n";

#[test]
fn the_invoker_is_nil_in_a_host_fired_event() {
    let host = new_host();
    let listener = start(
        &host,
        "listener",
        &["seen"],
        &[(
            "main.lua",
            "local seen = 'unset'\n\
             AddEventHandler('evt', function() seen = tostring(GetInvokingResource()) end)\n\
             exports('seen', function() return seen end)\n",
        )],
    );

    host.trigger_event("evt", EventSource::Local, &[], None)
        .expect("trigger");
    assert_eq!(first(&listener, "seen"), Value::from("nil"));
}

#[test]
fn the_caller_is_the_invoker_inside_an_export_of_another_resource() {
    let host = new_host();
    let provider = start(
        &host,
        "provider",
        &["who"],
        &[(
            "main.lua",
            "exports('who', function() return tostring(GetInvokingResource()) end)\n",
        )],
    );
    let consumer = start(
        &host,
        "consumer",
        &["ask"],
        &[(
            "main.lua",
            "exports('ask', function() return exports.provider:who() end)\n",
        )],
    );

    assert_eq!(first(&consumer, "ask"), Value::from("consumer"));
    assert_eq!(first(&provider, "who"), Value::from("nil"));
}

#[test]
fn the_triggering_resource_is_the_invoker_in_a_trigger_event_handler() {
    let host = new_host();
    let emitter = start(&host, "emitter", &["fire"], &[("main.lua", EMITTER)]);
    let listener = start(
        &host,
        "listener",
        &["seen"],
        &[(
            "main.lua",
            "local seen = 'unset'\n\
             AddEventHandler('thing:happened', function() seen = tostring(GetInvokingResource()) end)\n\
             exports('seen', function() return seen end)\n",
        )],
    );

    first(&emitter, "fire");
    assert_eq!(first(&listener, "seen"), Value::from("emitter"));
}

#[test]
fn the_invoker_survives_a_wait_inside_the_handler() {
    let host = new_host();
    let emitter = start(&host, "emitter", &["fire"], &[("main.lua", EMITTER)]);
    let listener = start(
        &host,
        "listener",
        &["seen"],
        &[(
            "main.lua",
            "local seen = 'unset'\n\
             AddEventHandler('thing:happened', function()\n\
                 Wait(0)\n\
                 seen = tostring(GetInvokingResource())\n\
             end)\n\
             exports('seen', function() return seen end)\n",
        )],
    );

    first(&emitter, "fire");
    assert_eq!(first(&listener, "seen"), Value::from("unset"));
    tick(&host, Instant::now() + Duration::from_millis(1));
    assert_eq!(first(&listener, "seen"), Value::from("emitter"));
}

#[test]
fn a_thread_created_in_a_handler_inherits_the_invoker() {
    let host = new_host();
    let emitter = start(&host, "emitter", &["fire"], &[("main.lua", EMITTER)]);
    let listener = start(
        &host,
        "listener",
        &["seen"],
        &[(
            "main.lua",
            "local seen = 'unset'\n\
             AddEventHandler('thing:happened', function()\n\
                 CreateThread(function() seen = tostring(GetInvokingResource()) end)\n\
             end)\n\
             exports('seen', function() return seen end)\n",
        )],
    );

    first(&emitter, "fire");
    tick(&host, Instant::now());
    assert_eq!(first(&listener, "seen"), Value::from("emitter"));
}

#[test]
fn the_invoker_is_restored_after_a_nested_export_fails() {
    let host = new_host();
    let emitter = start(&host, "emitter", &["fire"], &[("main.lua", EMITTER)]);
    let consumer = start(
        &host,
        "consumer",
        &["after", "inside"],
        &[(
            "main.lua",
            "local after, inside = 'unset', 'unset'\n\
             exports('boom', function() error('inside:' .. tostring(GetInvokingResource())) end)\n\
             AddEventHandler('thing:happened', function()\n\
                 local _, message = pcall(function() return exports.consumer:boom() end)\n\
                 inside = message\n\
                 after = tostring(GetInvokingResource())\n\
             end)\n\
             exports('after', function() return after end)\n\
             exports('inside', function() return inside end)\n",
        )],
    );

    first(&emitter, "fire");
    let inside = first(&consumer, "inside");
    assert!(
        inside
            .as_str()
            .is_some_and(|text| text.contains("inside:consumer")),
        "{inside:?}"
    );
    assert_eq!(first(&consumer, "after"), Value::from("emitter"));
}

#[test]
fn resource_functions_read_the_host() {
    let host = new_host();
    let _res = start(&host, "res", &[], &[("main.lua", "")]);
    let reader = start(
        &host,
        "reader",
        &["info"],
        &[(
            "main.lua",
            "exports('info', function()\n\
                 return {\n\
                     tostring(GetResourceState('res')),\n\
                     tostring(GetResourceState('nowhere')),\n\
                     tostring(GetNumResources()),\n\
                     tostring(GetResourceByFindIndex(0)),\n\
                     tostring(GetResourceByFindIndex(1)),\n\
                     tostring(GetResourceByFindIndex(2)),\n\
                     tostring(GetResourceByFindIndex(-1)),\n\
                     tostring(GetResourceMetadata('res', 'version', 0)),\n\
                     tostring(GetResourceMetadata('res', 'version', 1)),\n\
                     tostring(GetResourceMetadata('res', 'version', -1)),\n\
                     tostring(GetNumResourceMetadata('res', 'version')),\n\
                     tostring(GetNumResourceMetadata('res', 'absent')),\n\
                 }\n\
             end)\n",
        )],
    );
    host.states
        .borrow_mut()
        .insert("res".to_owned(), ResourceState::Stopping);
    host.metadata.borrow_mut().insert(
        "res".to_owned(),
        vec![
            ("version".to_owned(), "1.2.0".to_owned()),
            ("author".to_owned(), "someone".to_owned()),
        ],
    );

    let expected = [
        "stopping", "missing", "2", "reader", "res", "nil", "nil", "1.2.0", "nil", "nil", "1", "0",
    ];
    assert_eq!(
        first(&reader, "info"),
        Value::Array(expected.iter().map(|text| Value::from(*text)).collect())
    );
}

#[test]
fn restart_resource_delegates_to_server_host_and_is_server_only() {
    let host = new_host();
    let _target = start(&host, "target", &[], &[("main.lua", "")]);
    let controller = start(
        &host,
        "controller",
        &["restart"],
        &[(
            "main.lua",
            "exports('restart', function(name) return RestartResource(name) end)\n",
        )],
    );

    assert_eq!(
        call(&controller, "restart", &[Value::from("target")]),
        vec![Value::Bool(true)]
    );
    assert_eq!(
        call(&controller, "restart", &[Value::from("missing")]),
        vec![Value::Bool(false)]
    );
    assert_eq!(
        *host.restarted_resources.borrow(),
        vec![
            ("controller".to_owned(), "target".to_owned()),
            ("controller".to_owned(), "missing".to_owned()),
        ]
    );

    let client = new_client_host(1);
    let client_rt = start(
        &client,
        "client",
        &["probe"],
        &[(
            "main.lua",
            "exports('probe', function() return type(RestartResource) end)\n",
        )],
    );
    assert_eq!(first(&client_rt, "probe"), Value::from("nil"));
}
