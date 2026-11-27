//! Player functions: client events, the player list, names, endpoints and drops.

mod common;

use std::sync::Arc;

use common::{TestHost, call, first, new_client_host, new_host, start};
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

/// Gives player `id` the identifiers `identifiers`, in order.
fn identify(host: &TestHost, id: u32, identifiers: &[&str]) {
    let list = identifiers.iter().map(|text| (*text).to_owned()).collect();
    host.identifiers.borrow_mut().push((id, list));
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

const IDENTIFIER_EXPORTS: &str = "exports('list', function(p) return GetPlayerIdentifiers(p) end)\n\
     exports('count', function(p) return GetNumPlayerIdentifiers(p) end)\n\
     exports('at', function(p, i) return GetPlayerIdentifier(p, i) end)\n\
     exports('byType', function(p, kind) return GetPlayerIdentifierByType(p, kind) end)\n";

#[test]
fn identifier_functions_list_count_index_and_filter_by_type() {
    let host = new_host();
    let rt = start(
        &host,
        "ids",
        &["list", "count", "at", "byType"],
        &[("main.lua", IDENTIFIER_EXPORTS)],
    );
    join(&host, &[(7, "alice")]);
    identify(&host, 7, &["license:abc", "ip:127.0.0.1"]);

    assert_eq!(
        call(&rt, "list", &[Value::Int(7)]),
        vec![Value::Array(vec![
            Value::from("license:abc"),
            Value::from("ip:127.0.0.1"),
        ])]
    );
    assert_eq!(
        call(&rt, "list", &[Value::from("7")]),
        vec![Value::Array(vec![
            Value::from("license:abc"),
            Value::from("ip:127.0.0.1"),
        ])]
    );
    assert_eq!(call(&rt, "count", &[Value::Int(7)]), vec![Value::Int(2)]);
    assert_eq!(
        call(&rt, "at", &[Value::Int(7), Value::Int(0)]),
        vec![Value::from("license:abc")]
    );
    assert_eq!(
        call(&rt, "at", &[Value::Int(7), Value::Int(1)]),
        vec![Value::from("ip:127.0.0.1")]
    );
    assert_eq!(
        call(&rt, "at", &[Value::Int(7), Value::Int(2)]),
        vec![Value::Nil]
    );
    assert_eq!(
        call(&rt, "at", &[Value::Int(7), Value::Int(-1)]),
        vec![Value::Nil]
    );
    assert_eq!(
        call(&rt, "byType", &[Value::Int(7), Value::from("license")]),
        vec![Value::from("license:abc")]
    );
    assert_eq!(
        call(&rt, "byType", &[Value::Int(7), Value::from("ip")]),
        vec![Value::from("ip:127.0.0.1")]
    );
    assert_eq!(
        call(&rt, "byType", &[Value::Int(7), Value::from("steam")]),
        vec![Value::Nil]
    );
}

#[test]
fn identifier_functions_answer_nothing_for_an_unknown_player() {
    let host = new_host();
    let rt = start(
        &host,
        "ids",
        &["list", "count", "at", "byType"],
        &[("main.lua", IDENTIFIER_EXPORTS)],
    );
    join(&host, &[(7, "alice")]);
    identify(&host, 7, &["license:abc"]);

    assert_eq!(
        call(&rt, "list", &[Value::Int(9)]),
        vec![Value::Array(Vec::new())]
    );
    assert_eq!(call(&rt, "count", &[Value::Int(9)]), vec![Value::Int(0)]);
    assert_eq!(
        call(&rt, "at", &[Value::Int(9), Value::Int(0)]),
        vec![Value::Nil]
    );
    assert_eq!(
        call(&rt, "byType", &[Value::Int(9), Value::from("license")]),
        vec![Value::Nil]
    );
    assert_eq!(
        call(&rt, "list", &[Value::from("seven")]),
        vec![Value::Array(Vec::new())]
    );
}

#[test]
fn player_ace_check_uses_the_player_principal_and_is_false_for_unknown_players() {
    let host = new_host();
    let rt = start(
        &host,
        "aces",
        &["check"],
        &[(
            "main.lua",
            "exports('check', function(player, object) return IsPlayerAceAllowed(player, object) end)\n",
        )],
    );
    join(&host, &[(7, "alice")]);
    host.grants
        .borrow_mut()
        .push(("player.7".to_owned(), "command.secret".to_owned()));

    assert_eq!(
        call(
            &rt,
            "check",
            &[Value::Int(7), Value::from("command.secret")]
        ),
        vec![Value::Bool(true)]
    );
    assert_eq!(
        call(&rt, "check", &[Value::Int(7), Value::from("command.quit")]),
        vec![Value::Bool(false)]
    );
    assert_eq!(
        call(
            &rt,
            "check",
            &[Value::Int(9), Value::from("command.secret")]
        ),
        vec![Value::Bool(false)]
    );
    assert_eq!(
        call(
            &rt,
            "check",
            &[Value::from("nobody"), Value::from("command.secret")]
        ),
        vec![Value::Bool(false)]
    );
}

#[test]
fn principal_ace_check_passes_the_names_to_the_host() {
    let host = new_host();
    let rt = start(
        &host,
        "principals",
        &["check"],
        &[(
            "main.lua",
            "exports('check', function(principal, object) return IsPrincipalAceAllowed(principal, object) end)\n",
        )],
    );
    host.grants
        .borrow_mut()
        .push(("group.admin".to_owned(), "command".to_owned()));

    assert_eq!(
        call(
            &rt,
            "check",
            &[Value::from("group.admin"), Value::from("command")]
        ),
        vec![Value::Bool(true)]
    );
    assert_eq!(
        call(
            &rt,
            "check",
            &[Value::from("group.user"), Value::from("command")]
        ),
        vec![Value::Bool(false)]
    );
}

#[test]
fn ace_checks_are_server_only() {
    let host = new_client_host(2);
    let rt = start(
        &host,
        "clientside",
        &["probe"],
        &[(
            "main.lua",
            "exports('probe', function() return type(IsPlayerAceAllowed), type(IsPrincipalAceAllowed) end)\n",
        )],
    );

    assert_eq!(
        call(&rt, "probe", &[]),
        vec![Value::from("nil"), Value::from("nil")]
    );
}

#[test]
fn does_player_exist_checks_connected_players_and_is_server_only() {
    let host = new_host();
    let rt = start(
        &host,
        "exists",
        &["check"],
        &[(
            "main.lua",
            "exports('check', function(p) return DoesPlayerExist(p) end)\n",
        )],
    );
    join(&host, &[(7, "alice")]);

    for valid in [Value::Int(7), Value::from("7"), Value::Float(7.0)] {
        assert_eq!(call(&rt, "check", &[valid]), vec![Value::Bool(true)]);
    }
    for absent in [
        Value::Int(9),
        Value::Int(-1),
        Value::from("nobody"),
        Value::Float(7.5),
        Value::Nil,
    ] {
        assert_eq!(call(&rt, "check", &[absent]), vec![Value::Bool(false)]);
    }

    let client = new_client_host(7);
    let client_rt = start(
        &client,
        "clientside",
        &["probe"],
        &[(
            "main.lua",
            "exports('probe', function() return type(DoesPlayerExist) end)\n",
        )],
    );
    assert_eq!(first(&client_rt, "probe"), Value::from("nil"));
}

#[test]
fn trigger_client_and_latent_events_accept_batch_target_arrays_and_reject_invalid_entries() {
    let host = new_host();
    let rt = start(
        &host,
        "batch",
        &["send", "send_latent", "bad", "bad_latent"],
        &[(
            "main.lua",
            "exports('send', function(targets) TriggerClientEvent('ev', targets, 'hi', 42) end)\n\
             exports('send_latent', function(targets) TriggerLatentClientEvent('lev', targets, 5000, 'data') end)\n\
             exports('bad', function(targets) return pcall(TriggerClientEvent, 'ev', targets, 'hi') end)\n\
             exports('bad_latent', function(targets) return pcall(TriggerLatentClientEvent, 'lev', targets, 5000, 'data') end)\n",
        )],
    );

    call(
        &rt,
        "send",
        &[Value::Array(vec![Value::Int(1), Value::from("2")])],
    );
    call(&rt, "send", &[Value::Array(Vec::new())]);
    assert_eq!(
        *host.client_events.borrow(),
        vec![
            (
                "ev".to_owned(),
                ClientTarget::Player(1),
                vec![Value::from("hi"), Value::Int(42)],
            ),
            (
                "ev".to_owned(),
                ClientTarget::Player(2),
                vec![Value::from("hi"), Value::Int(42)],
            ),
        ]
    );

    call(
        &rt,
        "send_latent",
        &[Value::Array(vec![Value::from("3"), Value::Int(4)])],
    );
    call(&rt, "send_latent", &[Value::Array(Vec::new())]);
    assert_eq!(
        *host.latent_client_events.borrow(),
        vec![
            (
                "lev".to_owned(),
                ClientTarget::Player(3),
                vec![Value::from("data")],
                5000,
            ),
            (
                "lev".to_owned(),
                ClientTarget::Player(4),
                vec![Value::from("data")],
                5000,
            ),
        ]
    );

    host.client_events.borrow_mut().clear();
    host.latent_client_events.borrow_mut().clear();

    let bad_targets = [
        Value::Array(vec![Value::Int(1), Value::Int(-1)]),
        Value::Array(vec![Value::Int(-2)]),
        Value::Array(vec![Value::Int(1), Value::from("abc")]),
        Value::Map(vec![(Value::from("a"), Value::Int(1))]),
        Value::Map(vec![
            (Value::Int(1), Value::Int(1)),
            (Value::Int(3), Value::Int(2)),
        ]),
    ];
    for target in bad_targets {
        let out = call(&rt, "bad", std::slice::from_ref(&target));
        assert_eq!(out[0], Value::Bool(false));
        assert!(error_text(&out[1]).contains("expected a player id"));

        let latent_out = call(&rt, "bad_latent", &[target]);
        assert_eq!(latent_out[0], Value::Bool(false));
        assert!(error_text(&latent_out[1]).contains("expected a player id"));
    }

    assert!(host.client_events.borrow().is_empty());
    assert!(host.latent_client_events.borrow().is_empty());
}
