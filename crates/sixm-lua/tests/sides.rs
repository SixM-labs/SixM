//! Side-aware globals: what a server runtime and a client runtime each expose,
//! the client's connection to the server, and how server events reach a client.

mod common;

use std::rc::Rc;

use common::{TestHost, call, first, new_client_host, new_host, start};
use sixm_core::{EventContext, EventSource, GLOBAL_BAG, Host, ScriptRuntime, Value};
use sixm_lua::LuaRuntime;

/// Globals that exist only on servers.
const SERVER_ONLY: [&str; 7] = [
    "TriggerClientEvent",
    "TriggerLatentClientEvent",
    "GetPlayers",
    "GetNumPlayerIndices",
    "GetPlayerName",
    "GetPlayerPing",
    "DropPlayer",
];

/// Globals that exist only on clients.
const CLIENT_ONLY: [&str; 4] = [
    "TriggerServerEvent",
    "TriggerLatentServerEvent",
    "PlayerId",
    "LocalPlayer",
];

/// A runtime whose `present(name)` export reports whether the global `name` exists.
fn probe(host: &Rc<TestHost>) -> Rc<LuaRuntime> {
    start(
        host,
        "probe",
        &["present", "duplicity"],
        &[(
            "main.lua",
            "exports('present', function(name) return _G[name] ~= nil end)\n\
             exports('duplicity', function() return IsDuplicityVersion() end)\n",
        )],
    )
}

/// Whether the global `name` exists in `runtime`.
fn present(runtime: &LuaRuntime, name: &str) -> bool {
    call(runtime, "present", &[Value::from(name)]) == [Value::Bool(true)]
}

#[test]
fn server_runtime_has_server_globals_and_no_client_globals() {
    let host = new_host();
    let rt = probe(&host);

    for name in SERVER_ONLY.iter().chain(["Player"].iter()) {
        assert!(present(&rt, name), "`{name}` should exist on a server");
    }
    for name in CLIENT_ONLY.iter().chain(["GetPlayerServerId"].iter()) {
        assert!(!present(&rt, name), "`{name}` should not exist on a server");
    }
    for name in ["GlobalState", "AddEventHandler", "IsDuplicityVersion"] {
        assert!(present(&rt, name), "`{name}` should exist on a server");
    }
    assert_eq!(first(&rt, "duplicity"), Value::Bool(true));
}

#[test]
fn client_runtime_has_client_globals_and_no_server_globals() {
    let host = new_client_host(7);
    let rt = probe(&host);

    for name in CLIENT_ONLY.iter().chain(["GetPlayerServerId"].iter()) {
        assert!(present(&rt, name), "`{name}` should exist on a client");
    }
    for name in SERVER_ONLY.iter().chain(["Player"].iter()) {
        assert!(!present(&rt, name), "`{name}` should not exist on a client");
    }
    for name in ["GlobalState", "AddEventHandler", "IsDuplicityVersion"] {
        assert!(present(&rt, name), "`{name}` should exist on a client");
    }
    assert_eq!(first(&rt, "duplicity"), Value::Bool(false));
}

#[test]
fn trigger_server_event_records_name_and_arguments_and_refuses_functions() {
    let host = new_client_host(7);
    let rt = start(
        &host,
        "sender",
        &["send", "refuse"],
        &[(
            "main.lua",
            "exports('send', function()\n\
               TriggerServerEvent('ev', 'text', 1, 2.5, true, { 4, 'x' })\n\
             end)\n\
             exports('refuse', function() return pcall(TriggerServerEvent, 'ev', function() end) end)\n",
        )],
    );

    call(&rt, "send", &[]);
    assert_eq!(
        *host.server_events.borrow(),
        vec![(
            "ev".to_owned(),
            vec![
                Value::from("text"),
                Value::Int(1),
                Value::Float(2.5),
                Value::Bool(true),
                Value::Array(vec![Value::Int(4), Value::from("x")]),
            ],
        )]
    );

    let refused = call(&rt, "refuse", &[]);
    assert_eq!(refused[0], Value::Bool(false));
    let message = refused[1].as_str().unwrap_or_default();
    assert!(message.contains("functions cannot be sent over the network"));
    assert_eq!(
        host.server_events.borrow().len(),
        1,
        "refused call is not sent"
    );
}

#[test]
fn latent_event_globals_use_the_expected_argument_order_and_ping_defaults() {
    let client_host = new_client_host(7);
    let client_rt = start(
        &client_host,
        "latent-client",
        &["send"],
        &[(
            "main.lua",
            "exports('send', function(bps) return pcall(TriggerLatentServerEvent, 'latent', bps, 'payload') end)\n",
        )],
    );
    call(&client_rt, "send", &[Value::Int(0)]);
    assert_eq!(
        client_host.latent_server_events.borrow()[0],
        ("latent".to_owned(), vec![Value::from("payload")], 25_000)
    );

    let large_rate = 9_007_199_254_740_993;
    call(&client_rt, "send", &[Value::Int(large_rate)]);
    assert_eq!(
        client_host.latent_server_events.borrow()[1].2,
        large_rate.unsigned_abs()
    );
    for bad_rate in [
        Value::Float(f64::NEG_INFINITY),
        Value::Float(1.25),
        Value::from("1"),
    ] {
        assert_eq!(call(&client_rt, "send", &[bad_rate])[0], Value::Bool(false));
    }
    assert_eq!(client_host.latent_server_events.borrow().len(), 2);

    let server_host = new_host();
    let server_rt = start(
        &server_host,
        "latent-server",
        &["send", "ping"],
        &[(
            "main.lua",
            "exports('send', function() TriggerLatentClientEvent('latent', -1, -4, 'payload') end)\nexports('ping', function() return GetPlayerPing(999) end)\n",
        )],
    );
    call(&server_rt, "send", &[]);
    assert_eq!(
        server_host.latent_client_events.borrow()[0],
        (
            "latent".to_owned(),
            sixm_core::ClientTarget::All,
            vec![Value::from("payload")],
            25_000
        )
    );
    assert_eq!(first(&server_rt, "ping"), Value::Int(-1));
}

#[test]
fn player_id_is_zero_and_the_server_id_of_zero_is_the_local_server_id() {
    let host = new_client_host(7);
    let rt = start(
        &host,
        "ids",
        &["handle", "serverId", "own"],
        &[(
            "main.lua",
            "exports('handle', function() return PlayerId() end)\n\
             exports('serverId', function(p) return GetPlayerServerId(p) end)\n\
             exports('own', function() return GetPlayerServerId(PlayerId()) end)\n",
        )],
    );

    assert_eq!(first(&rt, "handle"), Value::Int(0));
    assert_eq!(first(&rt, "own"), Value::Int(7));
    assert_eq!(call(&rt, "serverId", &[Value::Int(0)]), vec![Value::Int(7)]);
    assert_eq!(call(&rt, "serverId", &[Value::Int(3)]), vec![Value::Nil]);
    assert_eq!(call(&rt, "serverId", &[Value::from("x")]), vec![Value::Nil]);
}

#[test]
fn local_player_state_reads_the_local_player_bag() {
    let host = new_client_host(7);
    host.state.borrow_mut().insert(
        ("player:7".to_owned(), "ready".to_owned()),
        Value::Bool(true),
    );
    let rt = start(
        &host,
        "local",
        &["ready"],
        &[(
            "main.lua",
            "exports('ready', function() return LocalPlayer.state.ready end)\n",
        )],
    );

    assert_eq!(first(&rt, "ready"), Value::Bool(true));
}

#[test]
fn global_state_writes_reach_the_host_on_a_client() {
    let host = new_client_host(7);
    let rt = start(
        &host,
        "writer",
        &["set"],
        &[(
            "main.lua",
            "exports('set', function() GlobalState.speed = 10 end)\n",
        )],
    );

    call(&rt, "set", &[]);
    assert_eq!(host.state_get(GLOBAL_BAG, "speed"), Some(Value::Int(10)));
}

#[test]
fn server_event_reaches_only_net_event_handlers_and_sees_source_65535() {
    let host = new_client_host(7);
    let rt = start(
        &host,
        "receiver",
        &["plain", "net", "seen"],
        &[(
            "main.lua",
            "local plain, net, seen\n\
             AddEventHandler('plainOnly', function() plain = true end)\n\
             RegisterNetEvent('fromServer', function(text)\n\
               net = text\n\
               seen = source\n\
             end)\n\
             exports('plain', function() return plain end)\n\
             exports('net', function() return net end)\n\
             exports('seen', function() return seen end)\n",
        )],
    );
    let ctx = EventContext::new();

    rt.deliver_event("plainOnly", EventSource::Server, &[], &ctx)
        .expect("deliver plain");
    assert_eq!(first(&rt, "plain"), Value::Nil);

    rt.deliver_event(
        "fromServer",
        EventSource::Server,
        &[Value::from("hello")],
        &ctx,
    )
    .expect("deliver net");
    assert_eq!(first(&rt, "net"), Value::from("hello"));
    assert_eq!(first(&rt, "seen"), Value::Int(65_535));
}
