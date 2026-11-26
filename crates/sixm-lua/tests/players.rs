//! Player functions: client events, the player list, names, endpoints and drops.

mod common;

use std::sync::Arc;

use common::{TestHost, call, first, new_host, start};
use sixm_core::{
    ClientTarget, EventContext, EventSource, FuncRef, PlayerInfo, ScriptError, ScriptRuntime,
    ServerHost, Value,
};

/// Replaces the host's player list with `(id, name)` pairs, each on its own address.
fn join(host: &TestHost, players: &[(u32, &str)]) {
    let list = players
        .iter()
        .map(|(id, name)| {
            let info = PlayerInfo {
                name: (*name).to_owned(),
                endpoint: format!("203.0.113.{id}:51234"),
            };
            (*id, info)
        })
        .collect();
    host.players.replace(list);
}

/// The text of an error value that a `pcall` caught.
fn error_text(value: &Value) -> String {
    value.as_str().unwrap_or_default().to_owned()
}

#[test]
fn client_event_reaches_the_target_with_its_arguments() {
    let host = new_host();
    let rt = start(
        &host,
        "sender",
        &["send"],
        &[(
            "main.lua",
            "exports('send', function()\n\
               TriggerClientEvent('ev', 3, 'text', 1, 2.5, true, { 4, 'x' })\n\
             end)\n",
        )],
    );

    call(&rt, "send", &[]);

    assert_eq!(
        *host.client_events.borrow(),
        vec![(
            "ev".to_owned(),
            ClientTarget::Player(3),
            vec![
                Value::from("text"),
                Value::Int(1),
                Value::Float(2.5),
                Value::Bool(true),
                Value::Array(vec![Value::Int(4), Value::from("x")]),
            ],
        )]
    );
}

#[test]
fn client_target_accepts_numbers_strings_and_minus_one_for_everyone() {
    let host = new_host();
    let rt = start(
        &host,
        "targets",
        &["send"],
        &[(
            "main.lua",
            "exports('send', function(target) TriggerClientEvent('ev', target) end)\n",
        )],
    );

    for target in [Value::from("3"), Value::from("-1"), Value::Int(-1)] {
        call(&rt, "send", &[target]);
    }

    let targets: Vec<ClientTarget> = host
        .client_events
        .borrow()
        .iter()
        .map(|(_, target, _)| *target)
        .collect();
    assert_eq!(
        targets,
        vec![
            ClientTarget::Player(3),
            ClientTarget::All,
            ClientTarget::All
        ]
    );
}

#[test]
fn invalid_targets_and_function_arguments_raise_catchable_errors() {
    let host = new_host();
    let rt = start(
        &host,
        "refusals",
        &["func", "nested", "badTarget"],
        &[(
            "main.lua",
            "exports('func', function() return pcall(TriggerClientEvent, 'ev', -1, function() end) end)\n\
             exports('nested', function() return pcall(TriggerClientEvent, 'ev', -1, { handler = function() end }) end)\n\
             exports('badTarget', function() return pcall(TriggerClientEvent, 'ev', 'abc') end)\n",
        )],
    );

    let func = call(&rt, "func", &[]);
    assert_eq!(func[0], Value::Bool(false));
    assert!(error_text(&func[1]).contains("functions cannot be sent over the network"));

    let nested = call(&rt, "nested", &[]);
    assert_eq!(nested[0], Value::Bool(false));
    assert!(error_text(&nested[1]).contains("functions cannot be sent over the network"));

    let bad = call(&rt, "badTarget", &[]);
    assert_eq!(bad[0], Value::Bool(false));
    assert!(error_text(&bad[1]).contains("expected a player id"));

    assert!(host.client_events.borrow().is_empty());
}

#[test]
fn host_refuses_function_references_too() {
    let host = new_host();
    let func = FuncRef {
        owner: Arc::from("owner"),
        instance: 1,
        id: 0,
    };

    let outcome = host.trigger_client_event(
        "ev",
        ClientTarget::All,
        &[Value::Array(vec![Value::FuncRef(func)])],
    );

    assert!(matches!(outcome, Err(ScriptError::Runtime(_))));
    assert!(host.client_events.borrow().is_empty());
}

#[test]
fn get_players_returns_ids_as_strings_in_host_order() {
    let host = new_host();
    let rt = start(
        &host,
        "roster",
        &["list", "count"],
        &[(
            "main.lua",
            "exports('list', function() return GetPlayers() end)\n\
             exports('count', function() return GetNumPlayerIndices() end)\n",
        )],
    );
    join(&host, &[(7, "alice"), (2, "bob")]);

    assert_eq!(
        first(&rt, "list"),
        Value::Array(vec![Value::from("7"), Value::from("2")])
    );
    assert_eq!(first(&rt, "count"), Value::Int(2));
}

#[test]
fn get_player_name_and_endpoint_accept_numbers_and_strings() {
    let host = new_host();
    let rt = start(
        &host,
        "names",
        &["name", "endpoint"],
        &[(
            "main.lua",
            "exports('name', function(p) return GetPlayerName(p) end)\n\
             exports('endpoint', function(p) return GetPlayerEndpoint(p) end)\n",
        )],
    );
    join(&host, &[(7, "alice"), (2, "bob")]);

    assert_eq!(
        call(&rt, "name", &[Value::Int(7)]),
        vec![Value::from("alice")]
    );
    assert_eq!(
        call(&rt, "name", &[Value::from("7")]),
        vec![Value::from("alice")]
    );
    assert_eq!(
        call(&rt, "name", &[Value::Float(2.0)]),
        vec![Value::from("bob")]
    );
    assert_eq!(call(&rt, "name", &[Value::Int(99)]), vec![Value::Nil]);
    assert_eq!(call(&rt, "name", &[Value::from("seven")]), vec![Value::Nil]);
    assert_eq!(call(&rt, "name", &[Value::Float(1.5)]), vec![Value::Nil]);
    assert_eq!(
        call(&rt, "endpoint", &[Value::from("2")]),
        vec![Value::from("203.0.113.2:51234")]
    );
    assert_eq!(call(&rt, "endpoint", &[Value::Int(99)]), vec![Value::Nil]);
}

#[test]
fn drop_player_uses_a_default_reason_and_accepts_a_custom_one() {
    let host = new_host();
    let rt = start(
        &host,
        "drops",
        &["drop", "dropBad"],
        &[(
            "main.lua",
            "exports('drop', function(p, reason) DropPlayer(p, reason) end)\n\
             exports('dropBad', function() return pcall(DropPlayer, 'nobody') end)\n",
        )],
    );

    call(&rt, "drop", &[Value::Int(4)]);
    call(&rt, "drop", &[Value::from("5"), Value::from("bye")]);
    let bad = call(&rt, "dropBad", &[]);
    assert_eq!(bad[0], Value::Bool(false));

    assert_eq!(
        *host.dropped.borrow(),
        vec![(4, "Dropped by server".to_owned()), (5, "bye".to_owned()),]
    );
}

#[test]
fn net_event_source_is_a_number_that_player_functions_accept() {
    let host = new_host();
    let rt = start(
        &host,
        "net",
        &["kind"],
        &[(
            "main.lua",
            "local kind\n\
             RegisterNetEvent('hello', function()\n\
               kind = type(source)\n\
               TriggerClientEvent('reply', source, GetPlayerName(source))\n\
             end)\n\
             exports('kind', function() return kind end)\n",
        )],
    );
    join(&host, &[(7, "alice")]);
    let ctx = EventContext::new();

    rt.deliver_event("hello", EventSource::Net(7), &[], &ctx)
        .expect("deliver");

    assert_eq!(first(&rt, "kind"), Value::from("number"));
    assert_eq!(
        *host.client_events.borrow(),
        vec![(
            "reply".to_owned(),
            ClientTarget::Player(7),
            vec![Value::from("alice")],
        )]
    );
}
