mod common;

use common::{call, new_client_host, new_host, start};
use sixm_core::{
    EntityInfo, EntityKind, PlayerInfo, ScriptRuntime, Value, WebReply, WebRequest,
};

#[test]
fn players_entities_and_buckets_work_on_server_and_client() {
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
    srv_host.grants.borrow_mut().push(("player.1".to_owned(), "admin.kick".to_owned()));

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
    assert_eq!(
        call(&rt, "result", &[]),
        vec![Value::from("200:pong:ok:")]
    );

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
    assert!(matches!(
        &replies[0].1,
        WebReply::Head { status: 201, .. }
    ));
    assert!(matches!(
        &replies[1].1,
        WebReply::Write(chunk) if chunk == b"hi "
    ));
    assert!(matches!(
        &replies[2].1,
        WebReply::End(chunk) if chunk == b"POST:/echo:a=1:AB"
    ));
}
