mod common;

use std::time::Duration;

use common::{call, new_client_host, new_host, start};
use sixm_core::{
    ClientTarget, ConvarKind, EntityInfo, EntityKind, PlayerInfo, ScriptRuntime, Value, WebReply,
    WebRequest,
};

#[test]
fn players_entities_and_buckets_work_on_server() {
    let srv_host = new_host();
    srv_host.players.borrow_mut().push((
        1,
        PlayerInfo {
            name: "Alice".to_owned(),
            endpoint: "127.0.0.1:40000".to_owned(),
        },
    ));
    srv_host
        .identifiers
        .borrow_mut()
        .push((1, vec!["license:abc".to_owned(), "ip:127.0.0.1".to_owned()]));
    srv_host
        .grants
        .borrow_mut()
        .push(("player.1".to_owned(), "admin.kick".to_owned()));

    let srv = start(
        &srv_host,
        "world",
        &["check"],
        &[(
            "index.js",
            r#"
            exports("check", () => {
                const v = CreateVehicle("adder", 1, 2, 3, 90, true, false);
                SetEntityCoords(v, 4, 5, 6);
                SetEntityHeading(v, 180);
                SetEntityOwner(v, 1);
                SetEntityOrphanMode(v, "migrate");
                SetPlayerRoutingBucket(1, 2);
                SetEntityRoutingBucket(v, 2);
                SetRoutingBucketPopulationEnabled(2, false);
                SetRoutingBucketEntityLockdownMode(2, "relaxed");
                const c = GetEntityCoords(v);
                return [
                    getPlayers()[0],
                    GetPlayerName(1),
                    GetPlayerPing(1),
                    GetPlayerIdentifierByType(1, "license"),
                    IsPlayerAceAllowed(1, "admin.kick"),
                    c.x, c.y, c.z,
                    GetEntityHeading(v),
                    NetworkGetEntityOwner(v),
                    GetPlayerRoutingBucket(1),
                    GetEntityRoutingBucket(v),
                    GetPlayersInRoutingBucket(2)[0],
                    GetRoutingBucketPopulationEnabled(2),
                    GetRoutingBucketEntityLockdownMode(2),
                    GetAllVehicles().length
                ];
            });
            "#,
        )],
    );

    assert_eq!(
        call(&srv, "check", &[]),
        vec![Value::Array(vec![
            Value::from("1"),
            Value::from("Alice"),
            Value::Int(-1),
            Value::from("license:abc"),
            Value::Bool(true),
            Value::Int(4),
            Value::Int(5),
            Value::Int(6),
            Value::Int(180),
            Value::Int(1),
            Value::Int(2),
            Value::Int(2),
            Value::Int(1),
            Value::Bool(false),
            Value::from("relaxed"),
            Value::Int(1),
        ])]
    );
}

#[test]
fn client_entity_natives_and_pool_work() {
    let cli_host = new_client_host(4);
    cli_host.local_ped.set(Some(99));
    cli_host.entities.borrow_mut().insert(
        99,
        EntityInfo {
            kind: EntityKind::Ped,
            model: 1,
            position: [0.0; 3],
            rotation: [0.0; 3],
            owner: Some(4),
        },
    );
    let cli = start(
        &cli_host,
        "cli_world",
        &["check"],
        &[(
            "index.js",
            r#"
            exports("check", () => {
                const v = CreateVehicle("adder", 1, 2, 3, 45, true, false);
                SetEntityCoords(v, 7, 8, 9);
                const pool = GetGamePool("CVehicle");
                return [
                    PlayerId(),
                    GetPlayerServerId(0),
                    PlayerPedId(),
                    v,
                    NetworkGetEntityOwner(v),
                    pool.length,
                    typeof DeleteEntity
                ];
            });
            "#,
        )],
    );
    assert_eq!(
        call(&cli, "check", &[]),
        vec![Value::Array(vec![
            Value::Int(0),
            Value::Int(4),
            Value::Int(99),
            Value::Int(1),
            Value::Int(4),
            Value::Int(1),
            Value::from("undefined"),
        ])]
    );
}

#[test]
fn http_client_and_web_handler_work() {
    let host = new_host();
    let rt = start(
        &host,
        "web_api",
        &["req", "result"],
        &[(
            "index.js",
            r#"
            let httpResult = "";
            exports("req", () => {
                PerformHttpRequest("https://example.test/api", (status, body, headers, err) => {
                    httpResult = `${status}:${body}:${headers["x-test"]}:${err}`;
                }, "POST", "ping", { "X-Req": "1" }, { timeout: 1500 });
            });
            exports("result", () => httpResult);

            SetHttpHandler((req, res) => {
                req.setDataHandler((body) => {
                    const got = typeof body === "string" ? body : Array.from(body).join(",");
                    res.writeHead(201, { "Content-Type": "text/plain" });
                    res.write("hi ");
                    res.send(`${req.method}:${req.path}:${req.query}:${got}`);
                });
            });
            "#,
        )],
    );

    call(&rt, "req", &[]);
    let (_, req, cb) = host.http_requests.borrow_mut().pop().expect("http request");
    assert_eq!(req.url, "https://example.test/api");
    assert_eq!(req.method, "POST");
    let headers = Value::Map(vec![(Value::from("x-test"), Value::from("ok"))]);
    rt.call_ref(
        cb.id,
        &[
            Value::Int(200),
            Value::from("pong"),
            headers,
            Value::from(""),
        ],
    )
    .expect("http cb");
    assert_eq!(call(&rt, "result", &[]), vec![Value::from("200:pong:ok:")]);

    let (_, web_cb) = host.web_handlers.borrow()[0].clone();
    let web_req = WebRequest {
        method: "POST".to_owned(),
        path: "/echo".to_owned(),
        query: "a=1".to_owned(),
        headers: vec![("host".to_owned(), "localhost".to_owned())],
        body: b"AB".to_vec(),
    };
    rt.call_web_handler(web_cb.id, 7, &web_req)
        .expect("call_web_handler");
    let replies = host.web_replies.borrow();
    assert!(matches!(&replies[0].1, WebReply::Head { status: 201, .. }));
    assert!(matches!(
        &replies[1].1,
        WebReply::Write(chunk) if chunk == b"hi "
    ));
    assert!(matches!(
        &replies[2].1,
        WebReply::End(chunk) if chunk == b"POST:/echo:a=1:AB"
    ));
}

#[test]
fn entity_state_natives_and_attachments_work() {
    let srv_host = new_host();
    let srv = start(
        &srv_host,
        "state_srv",
        &["check"],
        &[(
            "index.js",
            r#"
            exports("check", () => {
                const parent = CreateVehicle("adder", 0, 0, 0, 0, true, false);
                const child = CreateObject("prop_bin_01a", 1, 2, 3, true, false, false);
                FreezeEntityPosition(child, true);
                SetEntityVelocity(child, 3, 4, 5);
                SetEntityMaxHealth(child, 500);
                SetEntityHealth(child, 250);
                SetEntityVisible(child, false);
                SetEntityAlpha(child, 128);
                AttachEntityToEntity(child, parent, 2, 1, 0, 2, 0, 0, 90);
                const v = GetEntityVelocity(child);
                const attached = [
                    IsEntityPositionFrozen(child),
                    v.x, v.y, v.z,
                    GetEntityMaxHealth(child),
                    GetEntityHealth(child),
                    IsEntityVisible(child),
                    GetEntityAlpha(child),
                    IsEntityAttached(child),
                    GetEntityAttachedTo(child),
                ];
                ResetEntityAlpha(child);
                DetachEntity(child);
                return [
                    ...attached,
                    GetEntityAlpha(child),
                    IsEntityAttached(child),
                    GetEntityAttachedTo(child),
                ];
            });
            "#,
        )],
    );

    assert_eq!(
        call(&srv, "check", &[]),
        vec![Value::Array(vec![
            Value::Bool(true),
            Value::Int(3),
            Value::Int(4),
            Value::Int(5),
            Value::Int(500),
            Value::Int(250),
            Value::Bool(false),
            Value::Int(128),
            Value::Bool(true),
            Value::Int(1),
            Value::Int(255),
            Value::Bool(false),
            Value::Int(0),
        ])]
    );

    let cli_host = new_client_host(1);
    let cli = start(
        &cli_host,
        "state_cli",
        &["check"],
        &[(
            "index.js",
            r#"
            exports("check", () => {
                const v = CreateVehicle("adder", 0, 0, 0, 0, true, false);
                return [
                    typeof FreezeEntityPosition,
                    GetEntityHealth(v),
                    GetEntityMaxHealth(v),
                    IsEntityVisible(v),
                    GetEntityAlpha(v),
                    IsEntityPositionFrozen(v),
                    IsEntityAttached(v),
                ];
            });
            "#,
        )],
    );
    assert_eq!(
        call(&cli, "check", &[]),
        vec![Value::Array(vec![
            Value::from("undefined"),
            Value::Int(200),
            Value::Int(200),
            Value::Bool(true),
            Value::Int(255),
            Value::Bool(false),
            Value::Bool(false),
        ])]
    );
}

#[test]
fn does_player_exist_checks_connected_players_on_server_only() {
    let srv_host = new_host();
    srv_host.players.borrow_mut().push((
        2,
        PlayerInfo {
            name: "Bob".to_owned(),
            endpoint: "127.0.0.1:40002".to_owned(),
        },
    ));

    let srv = start(
        &srv_host,
        "exist_srv",
        &["check"],
        &[(
            "index.js",
            r#"
            exports("check", () => [
                DoesPlayerExist(2),
                DoesPlayerExist("2"),
                DoesPlayerExist(99),
                DoesPlayerExist(-1),
                DoesPlayerExist(null),
                DoesPlayerExist("nope"),
            ]);
            "#,
        )],
    );

    assert_eq!(
        call(&srv, "check", &[]),
        vec![Value::Array(vec![
            Value::Bool(true),
            Value::Bool(true),
            Value::Bool(false),
            Value::Bool(false),
            Value::Bool(false),
            Value::Bool(false),
        ])]
    );

    let cli_host = new_client_host(1);
    let cli = start(
        &cli_host,
        "exist_cli",
        &["check"],
        &[(
            "index.js",
            r#"exports("check", () => typeof DoesPlayerExist);"#,
        )],
    );
    assert_eq!(call(&cli, "check", &[]), vec![Value::from("undefined")]);
}

#[test]
fn batch_client_event_targets_support_arrays_and_reject_invalid_entries() {
    let srv_host = new_host();
    let srv = start(
        &srv_host,
        "batch_srv",
        &["send_valid", "send_invalid"],
        &[(
            "index.js",
            r#"
            exports("send_valid", () => {
                emitNet("evt:all", -1, "hi");
                emitNet("evt:one", 1, 10);
                emitNet("evt:batch_num", [1, 2], "a", true);
                TriggerClientEvent("evt:batch_str", ["1", "2"], 99);
                TriggerClientEvent("evt:empty", [], "ignored");
                TriggerLatentClientEvent("evt:latent_batch", [2, "1"], 50000, "payload");
                return true;
            });

            exports("send_invalid", () => {
                const fails = (fn) => {
                    try { fn(); return false; } catch (e) { return true; }
                };
                return [
                    fails(() => emitNet("evt:bad", [1, "bad"], 1)),
                    fails(() => TriggerClientEvent("evt:bad", [1, -1], 1)),
                    fails(() => TriggerClientEvent("evt:bad", [1, null], 1)),
                    fails(() => TriggerLatentClientEvent("evt:bad", [1, 1.5], 5000, 1)),
                ];
            });
            "#,
        )],
    );

    assert_eq!(call(&srv, "send_valid", &[]), vec![Value::Bool(true)]);
    assert_eq!(
        *srv_host.client_events.borrow(),
        vec![
            (
                "evt:all".to_owned(),
                ClientTarget::All,
                vec![Value::from("hi")]
            ),
            (
                "evt:one".to_owned(),
                ClientTarget::Player(1),
                vec![Value::Int(10)]
            ),
            (
                "evt:batch_num".to_owned(),
                ClientTarget::Player(1),
                vec![Value::from("a"), Value::Bool(true)]
            ),
            (
                "evt:batch_num".to_owned(),
                ClientTarget::Player(2),
                vec![Value::from("a"), Value::Bool(true)]
            ),
            (
                "evt:batch_str".to_owned(),
                ClientTarget::Player(1),
                vec![Value::Int(99)]
            ),
            (
                "evt:batch_str".to_owned(),
                ClientTarget::Player(2),
                vec![Value::Int(99)]
            ),
        ]
    );
    assert_eq!(
        *srv_host.latent_client_events.borrow(),
        vec![
            (
                "evt:latent_batch".to_owned(),
                ClientTarget::Player(2),
                vec![Value::from("payload")],
                50_000
            ),
            (
                "evt:latent_batch".to_owned(),
                ClientTarget::Player(1),
                vec![Value::from("payload")],
                50_000
            ),
        ]
    );

    let before_count = srv_host.client_events.borrow().len();
    let before_latent = srv_host.latent_client_events.borrow().len();
    assert_eq!(
        call(&srv, "send_invalid", &[]),
        vec![Value::Array(vec![Value::Bool(true); 4])]
    );
    assert_eq!(srv_host.client_events.borrow().len(), before_count);
    assert_eq!(srv_host.latent_client_events.borrow().len(), before_latent);
}

#[test]
fn ace_check_uses_the_calling_resources_principal() {
    let host = new_host();
    host.grants
        .borrow_mut()
        .push(("resource.aces".to_owned(), "command.x".to_owned()));
    let rt = start(
        &host,
        "aces",
        &["check"],
        &[(
            "index.js",
            r#"exports("check", (object) => IsAceAllowed(object));"#,
        )],
    );

    assert_eq!(
        call(&rt, "check", &[Value::from("command.x")]),
        vec![Value::Bool(true)]
    );
    assert_eq!(
        call(&rt, "check", &[Value::from("command.y")]),
        vec![Value::Bool(false)]
    );
}

#[test]
fn player_guid_time_online_index_and_host_id_work_on_server() {
    let host = new_host();
    host.players.borrow_mut().extend([
        (
            7,
            PlayerInfo {
                name: "a".to_owned(),
                endpoint: "203.0.113.7:51234".to_owned(),
            },
        ),
        (
            3,
            PlayerInfo {
                name: "b".to_owned(),
                endpoint: "203.0.113.3:51234".to_owned(),
            },
        ),
    ]);
    host.identifiers
        .borrow_mut()
        .push((7, vec!["license:abc".to_owned(), "ip:1.2.3.4".to_owned()]));
    host.time_online
        .borrow_mut()
        .insert(7, Duration::from_secs(125));
    let rt = start(
        &host,
        "ids",
        &["check", "host"],
        &[(
            "index.js",
            r#"
            exports("check", () => [
                GetPlayerGuid(7), GetPlayerGuid(3), GetPlayerGuid(99),
                GetPlayerTimeOnline(7), GetPlayerTimeOnline(99),
                GetPlayerFromIndex(0), GetPlayerFromIndex(1),
                GetPlayerFromIndex(2), GetPlayerFromIndex(-1),
                GetHostId()
            ]);
            exports("host", () => [GetHostId()]);
            "#,
        )],
    );

    assert_eq!(
        call(&rt, "check", &[]),
        vec![Value::Array(vec![
            Value::from("abc"),
            Value::Nil,
            Value::Nil,
            Value::Int(125),
            Value::Nil,
            Value::from("7"),
            Value::from("3"),
            Value::Nil,
            Value::Nil,
            Value::from("3"),
        ])]
    );

    host.players.borrow_mut().clear();
    assert_eq!(call(&rt, "host", &[]), vec![Value::Array(vec![Value::Nil])]);
}

#[test]
fn set_game_type_and_map_name_set_server_info_convars() {
    let host = new_host();
    let _rt = start(
        &host,
        "info",
        &[],
        &[("index.js", r#"SetGameType("race"); SetMapName("x");"#)],
    );

    let sets = host.convar_sets.borrow();
    assert!(sets.contains(&(
        "gametype".to_owned(),
        "race".to_owned(),
        ConvarKind::ServerInfo
    )));
    assert!(sets.contains(&("mapname".to_owned(), "x".to_owned(), ConvarKind::ServerInfo)));
}
