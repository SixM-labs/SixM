//! State bags from scripts: `GlobalState`, `Player(id).state`, change handlers
//! and `GetPlayerFromStateBagName`.

mod common;

use common::{TestHost, call, first, new_client_host, new_host, start, text};
use sixm_core::{EntityKind, EventSource, Host, PlayerInfo, ScriptRuntime, Value};

/// Replaces the host's player list with the given ids.
fn join(host: &TestHost, ids: &[u32]) {
    let players = ids
        .iter()
        .map(|id| {
            let info = PlayerInfo {
                name: format!("player {id}"),
                endpoint: format!("203.0.113.{id}:51234"),
            };
            (*id, info)
        })
        .collect();
    host.players.replace(players);
}

#[test]
fn global_state_reads_writes_and_deletes_across_resources() {
    let host = new_host();
    let writer = start(
        &host,
        "writer",
        &["run"],
        &[(
            "main.lua",
            "exports('run', function()\n\
               GlobalState.speed = 10\n\
               GlobalState.table = { 1, 'two' }\n\
               local kept = GlobalState.speed\n\
               GlobalState.speed = nil\n\
               return tostring(kept) .. ' ' .. tostring(GlobalState.speed)\n\
             end)\n",
        )],
    );
    let reader = start(
        &host,
        "reader",
        &["read"],
        &[(
            "main.lua",
            "exports('read', function(key)\n\
               local value = GlobalState[key]\n\
               if type(value) == 'table' then return value[2] end\n\
               return value\n\
             end)\n",
        )],
    );

    assert_eq!(text(&writer, "run"), "10 nil");
    assert_eq!(
        call(&reader, "read", &[Value::from("speed")]),
        vec![Value::Nil],
        "a deleted key reads as nil"
    );
    assert_eq!(
        call(&reader, "read", &[Value::from("table")]),
        vec![Value::from("two")],
        "tables are copied through the host"
    );
    assert_eq!(
        host.state_get("global", "table"),
        Some(Value::Array(vec![Value::Int(1), Value::from("two")]))
    );
}

#[test]
fn plain_assignment_replicates_and_set_chooses() {
    let host = new_host();
    let rt = start(
        &host,
        "replication",
        &["plain", "hidden", "default", "log"],
        &[(
            "main.lua",
            "LOG = {}\n\
             AddStateBagChangeHandler(nil, nil, function(bag, key, value, reserved, replicated)\n\
               LOG[#LOG + 1] = bag .. '.' .. key .. '=' .. tostring(value)\n\
                 .. ' ' .. tostring(replicated) .. ' ' .. tostring(reserved)\n\
             end)\n\
             exports('plain', function() GlobalState.a = 1 end)\n\
             exports('hidden', function() GlobalState:set('b', 2, false) end)\n\
             exports('default', function() GlobalState:set('c', 3) end)\n\
             exports('log', function() return table.concat(LOG, ';') end)\n",
        )],
    );

    call(&rt, "plain", &[]);
    call(&rt, "hidden", &[]);
    call(&rt, "default", &[]);
    assert_eq!(
        text(&rt, "log"),
        "global.a=1 true 0;global.b=2 false 0;global.c=3 true 0"
    );
    assert_eq!(host.state_get("global", "b"), Some(Value::Int(2)));
}

#[test]
fn player_state_accepts_numeric_and_string_ids() {
    let host = new_host();
    join(&host, &[1, 2]);
    let rt = start(
        &host,
        "players",
        &["set", "get"],
        &[(
            "main.lua",
            "exports('set', function(id, value) Player(id).state.color = value end)\n\
             exports('get', function(id) return Player(id).state.color end)\n",
        )],
    );

    call(&rt, "set", &[Value::Int(1), Value::from("red")]);
    call(&rt, "set", &[Value::from("2"), Value::from("blue")]);

    assert_eq!(
        host.state_get("player:1", "color"),
        Some(Value::from("red"))
    );
    assert_eq!(
        host.state_get("player:2", "color"),
        Some(Value::from("blue"))
    );
    assert_eq!(
        call(&rt, "get", &[Value::from("2")]),
        vec![Value::from("blue")]
    );
}

#[test]
fn writing_to_the_bag_of_an_unknown_player_is_an_error() {
    let host = new_host();
    join(&host, &[1]);
    let rt = start(
        &host,
        "unknown",
        &["write", "bad_id"],
        &[(
            "main.lua",
            "exports('write', function() Player(9).state.x = 1 end)\n\
             exports('bad_id', function() return Player('nobody').state end)\n",
        )],
    );

    let err = rt
        .call_export("write", &[], None)
        .expect_err("player 9 is not connected");
    assert!(
        err.to_string()
            .contains("state bag `player:9` does not exist"),
        "error: {err}"
    );
    let err = rt
        .call_export("bad_id", &[], None)
        .expect_err("a non-numeric id is refused");
    assert!(
        err.to_string().contains("expected a player id"),
        "error: {err}"
    );
    assert_eq!(host.state_get("player:9", "x"), None);
}

#[test]
fn functions_cannot_be_stored_in_state() {
    let host = new_host();
    let rt = start(
        &host,
        "functions",
        &["direct", "nested"],
        &[(
            "main.lua",
            "exports('direct', function() GlobalState.f = function() end end)\n\
             exports('nested', function() GlobalState.t = { function() end } end)\n",
        )],
    );

    for export in ["direct", "nested"] {
        let err = rt
            .call_export(export, &[], None)
            .expect_err("a function cannot be stored");
        assert!(
            err.to_string()
                .contains("functions cannot be sent over the network"),
            "{export}: {err}"
        );
    }
    assert_eq!(host.state_get("global", "f"), None);
    assert_eq!(host.state_get("global", "t"), None);
}

#[test]
fn change_handler_filters_match_key_bag_both_or_neither() {
    let host = new_host();
    join(&host, &[1]);
    let rt = start(
        &host,
        "filters",
        &["write", "write_other", "write_player", "log"],
        &[(
            "main.lua",
            "LOG = {}\n\
             local function note(name)\n\
               return function(bag, key) LOG[#LOG + 1] = name .. ':' .. bag .. '.' .. key end\n\
             end\n\
             AddStateBagChangeHandler('speed', nil, note('key'))\n\
             AddStateBagChangeHandler(nil, 'global', note('bag'))\n\
             AddStateBagChangeHandler('speed', 'global', note('both'))\n\
             AddStateBagChangeHandler(nil, nil, note('all'))\n\
             exports('write', function() GlobalState.speed = 1 end)\n\
             exports('write_other', function() GlobalState.other = 1 end)\n\
             exports('write_player', function() Player(1).state.speed = 1 end)\n\
             exports('log', function() return table.concat(LOG, ';') end)\n",
        )],
    );

    call(&rt, "write", &[]);
    assert_eq!(
        text(&rt, "log"),
        "key:global.speed;bag:global.speed;both:global.speed;all:global.speed"
    );

    call(&rt, "write_other", &[]);
    call(&rt, "write_player", &[]);
    assert_eq!(
        text(&rt, "log"),
        "key:global.speed;bag:global.speed;both:global.speed;all:global.speed;\
         bag:global.other;all:global.other;key:player:1.speed;all:player:1.speed"
    );
}

#[test]
fn change_handlers_receive_the_value_and_can_be_removed() {
    let host = new_host();
    let rt = start(
        &host,
        "cookies",
        &["cookie", "unhook", "count", "seen", "write"],
        &[(
            "main.lua",
            "COUNT = 0\n\
             SEEN = nil\n\
             COOKIE = AddStateBagChangeHandler(nil, nil, function(bag, key, value)\n\
               COUNT = COUNT + 1\n\
               SEEN = value\n\
             end)\n\
             exports('cookie', function() return COOKIE end)\n\
             exports('unhook', function() RemoveStateBagChangeHandler(COOKIE) end)\n\
             exports('count', function() return COUNT end)\n\
             exports('seen', function() return SEEN end)\n\
             exports('write', function(value) GlobalState.x = value end)\n",
        )],
    );

    assert!(matches!(first(&rt, "cookie"), Value::Int(_)));
    call(&rt, "write", &[Value::Int(5)]);
    assert_eq!(first(&rt, "count"), Value::Int(1));
    assert_eq!(first(&rt, "seen"), Value::Int(5));

    call(&rt, "unhook", &[]);
    call(&rt, "write", &[Value::Int(6)]);
    assert_eq!(
        first(&rt, "count"),
        Value::Int(1),
        "a removed handler stays silent"
    );
}

#[test]
fn handlers_in_other_resources_see_every_write() {
    let host = new_host();
    let writer = start(
        &host,
        "writer",
        &["write"],
        &[(
            "main.lua",
            "exports('write', function() GlobalState.n = 1 end)\n",
        )],
    );
    let listener = start(
        &host,
        "listener",
        &["count"],
        &[(
            "main.lua",
            "HITS = 0\n\
             AddStateBagChangeHandler('n', nil, function() HITS = HITS + 1 end)\n\
             exports('count', function() return HITS end)\n",
        )],
    );

    call(&writer, "write", &[]);
    call(&writer, "write", &[]);
    assert_eq!(first(&listener, "count"), Value::Int(2));
}

#[test]
fn player_bag_names_map_back_to_player_ids() {
    let host = new_host();
    let rt = start(
        &host,
        "owners",
        &["owner"],
        &[(
            "main.lua",
            "exports('owner', function(bag) return GetPlayerFromStateBagName(bag) end)\n",
        )],
    );

    for (bag, expected) in [
        ("player:3", 3),
        ("player:12", 12),
        ("global", 0),
        ("player:03", 0),
        ("player:x", 0),
    ] {
        assert_eq!(
            call(&rt, "owner", &[Value::from(bag)]),
            vec![Value::Int(expected)],
            "bag {bag}"
        );
    }
}

#[test]
fn a_write_inside_a_handler_keeps_that_handlers_source() {
    let host = new_host();
    let rt = start(
        &host,
        "sources",
        &["last"],
        &[(
            "main.lua",
            "AddStateBagChangeHandler(nil, nil, function() end)\n\
             AddEventHandler('probe', function()\n\
               GlobalState.touched = 1\n\
               SEEN = source\n\
             end)\n\
             exports('last', function() return SEEN end)\n",
        )],
    );

    host.trigger_event("probe", EventSource::Player(7), &[], None)
        .expect("the event is delivered");
    assert_eq!(first(&rt, "last"), Value::Int(7));
}

#[test]
fn state_bag_natives_read_write_and_list_keys_by_name() {
    let host = new_host();
    join(&host, &[1]);
    let rt = start(
        &host,
        "natives",
        &["write", "check", "clear", "after"],
        &[(
            "main.lua",
            "exports('write', function()\n\
               SetStateBagValue('global', 'b', 2, true)\n\
               SetStateBagValue('global', 'a', 'x', false)\n\
               Player(1).state.score = 3\n\
             end)\n\
             exports('check', function()\n\
               return table.concat({\n\
                 tostring(GetStateBagValue('global', 'a')),\n\
                 tostring(StateBagHasKey('global', 'b')),\n\
                 table.concat(GetStateBagKeys('global'), ','),\n\
                 tostring(GetStateBagValue('player:1', 'score')),\n\
               }, ' ')\n\
             end)\n\
             exports('clear', function()\n\
               SetStateBagValue('global', 'b', nil, true)\n\
             end)\n\
             exports('after', function()\n\
               return tostring(StateBagHasKey('global', 'b')) .. ' ' .. table.concat(GetStateBagKeys('global'), ',')\n\
             end)\n",
        )],
    );

    call(&rt, "write", &[]);
    assert_eq!(text(&rt, "check"), "x true a,b 3");
    assert_eq!(host.state_get("global", "a"), Some(Value::from("x")));
    call(&rt, "clear", &[]);
    assert_eq!(text(&rt, "after"), "false a");
}

#[test]
fn state_bag_natives_answer_nil_for_unknown_bags() {
    let host = new_host();
    let rt = start(
        &host,
        "unknown",
        &["probe"],
        &[(
            "main.lua",
            "exports('probe', function()\n\
               return table.concat({\n\
                 tostring(GetStateBagValue('player:9', 'x')),\n\
                 tostring(GetStateBagKeys('entity:99')),\n\
                 tostring(GetStateBagKeys('nonsense')),\n\
                 tostring(StateBagHasKey('player:9', 'x')),\n\
               }, ' ')\n\
             end)\n",
        )],
    );

    assert_eq!(text(&rt, "probe"), "nil nil nil false");
}

#[test]
fn set_state_bag_value_fails_like_a_bag_write() {
    let host = new_host();
    let rt = start(
        &host,
        "setter",
        &["unknown_bag", "function_value", "default", "read_default"],
        &[(
            "main.lua",
            "exports('unknown_bag', function()\n\
               return tostring(select(2, pcall(SetStateBagValue, 'player:9', 'x', 1, true)))\n\
             end)\n\
             exports('function_value', function()\n\
               return tostring(select(2, pcall(SetStateBagValue, 'global', 'f', function() end, false)))\n\
             end)\n\
             exports('default', function()\n\
               SetStateBagValue('global', 'd', 5)\n\
             end)\n\
             exports('read_default', function()\n\
               return GetStateBagValue('global', 'd')\n\
             end)\n",
        )],
    );

    assert!(text(&rt, "unknown_bag").contains("state bag `player:9` does not exist"));
    assert!(text(&rt, "function_value").contains("functions cannot be sent over the network"));
    call(&rt, "default", &[]);
    assert_eq!(first(&rt, "read_default"), Value::Int(5));
}

#[test]
fn ensure_entity_state_bag_is_accepted_and_entity_bags_list_empty() {
    let host = new_host();
    let id = host.create_entity_for_test(EntityKind::Vehicle, None);
    let rt = start(
        &host,
        "ensure",
        &["ensure"],
        &[(
            "main.lua",
            "exports('ensure', function(id)\n\
               EnsureEntityStateBag(id)\n\
               EnsureEntityStateBag(9999)\n\
               return #GetStateBagKeys('entity:' .. id)\n\
             end)\n",
        )],
    );

    assert_eq!(
        call(&rt, "ensure", &[Value::Int(i64::from(id))]),
        vec![Value::Int(0)]
    );
}

#[test]
fn state_bag_natives_exist_on_the_client() {
    let host = new_client_host(1);
    host.state_set("global", "x", Value::Int(4), false)
        .expect("a client writes local state");
    let rt = start(
        &host,
        "client_natives",
        &["read", "kind"],
        &[(
            "main.lua",
            "exports('read', function() return GetStateBagValue('global', 'x') end)\n\
             exports('kind', function() return type(SetStateBagValue) end)\n",
        )],
    );

    assert_eq!(first(&rt, "read"), Value::Int(4));
    assert_eq!(text(&rt, "kind"), "function");
}

#[test]
fn set_state_bag_value_decodes_msgpack_bytes_up_to_the_length() {
    let host = new_host();
    let rt = start(
        &host,
        "packed",
        &["write", "read", "trailing", "short", "bad"],
        &[(
            "main.lua",
            "exports('write', function()\n\
               local packed = msgpack.pack({ 1, 'two' })\n\
               SetStateBagValue('global', 'packed', packed, #packed, true)\n\
               local kept = msgpack.pack('kept')\n\
               SetStateBagValue('global', 'trailing', kept .. 'junk', #kept, false)\n\
             end)\n\
             exports('read', function()\n\
               return GetStateBagValue('global', 'packed')[2]\n\
             end)\n\
             exports('trailing', function()\n\
               return GetStateBagValue('global', 'trailing')\n\
             end)\n\
             exports('short', function()\n\
               local packed = msgpack.pack('x')\n\
               return tostring(select(2, pcall(SetStateBagValue, 'global', 'k', packed, #packed + 1, true)))\n\
             end)\n\
             exports('bad', function()\n\
               return tostring(pcall(SetStateBagValue, 'global', 'k', string.char(0xc1), 1, true))\n\
             end)\n",
        )],
    );

    call(&rt, "write", &[]);
    assert_eq!(text(&rt, "read"), "two");
    assert_eq!(first(&rt, "trailing"), Value::from("kept"));
    assert!(text(&rt, "short").contains("SetStateBagValue: valueLength exceeds data"));
    assert_eq!(text(&rt, "bad"), "false");
}
