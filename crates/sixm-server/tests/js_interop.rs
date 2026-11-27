//! End-to-end JavaScript runtime tests inside `sixm-server` and `sixm-client`:
//! pure JS resources, Lua <-> JS cross-runtime interop, mixed `.lua` + `.js`
//! resources, HTTP `/info.json` deduplication, and loopback client sync.

mod common;

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use common::{Fixture, manifest};
use sixm_client::{Client, ClientRuntime};
use sixm_core::{Host, Value};
use sixm_net::Identity;
use tempfile::TempDir;

const TIMEOUT: Duration = Duration::from_secs(15);
const STEP: Duration = Duration::from_millis(5);

fn exchange_get(addr: SocketAddr, path: &str) -> Option<(u16, String)> {
    let mut stream = TcpStream::connect(addr).ok()?;
    stream.set_read_timeout(Some(Duration::from_secs(5))).ok()?;
    let raw = format!("GET {path} HTTP/1.1\r\nHost: localhost\r\n\r\n");
    stream.write_all(raw.as_bytes()).ok()?;
    let mut data = Vec::new();
    stream.read_to_end(&mut data).ok()?;
    let split = data.windows(4).position(|w| w == b"\r\n\r\n")? + 4;
    let head = String::from_utf8_lossy(&data[..split]);
    let status = head.split_whitespace().nth(1)?.parse().ok()?;
    let body = String::from_utf8_lossy(&data[split..]).into_owned();
    Some((status, body))
}

#[test]
fn lua_and_js_resources_interoperate_and_cancel_events() {
    let fx = Fixture::new();
    fx.add_resource(
        "js_lib",
        &manifest(&["index.js"], &["ping", "withCallback"], &[]),
        &[(
            "index.js",
            r#"
            exports("ping", (who) => `${who}:${GetInvokingResource()}`);
            exports("withCallback", (n, cb) => cb(n * 2, GetInvokingResource()));

            on("interop:checkCancel", () => {
                CancelEvent();
            });

            AddStateBagChangeHandler("pair", "global", (bag, key, val) => {
                GlobalState.jsSaw = `${bag}:${key}:${val}`;
            });
            "#,
        )],
    );

    fx.add_resource(
        "lua_app",
        &manifest(&["server.lua"], &["double", "run"], &["js_lib"]),
        &[(
            "server.lua",
            r"
            exports('double', function(n)
                return n + 10, GetInvokingResource()
            end)

            exports('run', function()
                local p = exports.js_lib:ping('hello')
                local cb_out = ''
                exports.js_lib:withCallback(21, function(res, inv)
                    cb_out = tostring(res) .. ':' .. tostring(inv)
                end)
                TriggerEvent('interop:checkCancel')
                local canceled = WasEventCanceled()
                GlobalState.pair = 'fromLua'
                return p .. '|' .. cb_out .. '|' .. tostring(canceled) .. '|' .. tostring(GlobalState.jsSaw)
            end)
            ",
        )],
    );

    fx.add_resource(
        "js_caller",
        &manifest(&["caller.js"], &["callLua"], &["lua_app"]),
        &[(
            "caller.js",
            r#"
            exports("callLua", () => {
                const [val, inv] = exports.lua_app.double(5);
                return `${val}:${inv}`;
            });
            "#,
        )],
    );

    fx.manager().start("lua_app").expect("start lua_app");
    fx.manager().start("js_caller").expect("start js_caller");

    assert_eq!(
        fx.text("lua_app", "run"),
        "hello:lua_app|42:lua_app|true|global:pair:fromLua"
    );
    assert_eq!(fx.text("js_caller", "callLua"), "15:js_caller");
}

#[test]
fn mixed_lua_and_js_resource_loads_in_order_stops_cleanly_and_deduplicates_info_json() {
    let fx = Fixture::new();
    fx.add_resource(
        "mixed",
        &manifest(
            &["first.lua", "second.js", "third.lua"],
            &["fromLua", "fromJs", "trace"],
            &[],
        ),
        &[
            (
                "first.lua",
                r"
                GlobalState.seq = 'lua1'
                exports('fromLua', function() return 'lua-ok' end)
                AddEventHandler('onResourceStop', function(res)
                    if res == 'mixed' then GlobalState.stoppedLua = true end
                end)
                ",
            ),
            (
                "second.js",
                r#"
                GlobalState.seq = `${GlobalState.seq},js2`;
                exports("fromJs", () => "js-ok");
                on("onResourceStop", (res) => {
                    if (res === "mixed") GlobalState.stoppedJs = true;
                });
                "#,
            ),
            (
                "third.lua",
                r"
                GlobalState.seq = GlobalState.seq .. ',lua3'
                exports('trace', function() return GlobalState.seq end)
                ",
            ),
        ],
    );

    fx.server.execute("endpoint_add_udp 127.0.0.1:0");
    let (tx, rx) = mpsc::channel();
    let addr = fx.server.start_network(tx).expect("network");
    fx.server.execute("ensure mixed");

    assert_eq!(fx.text("mixed", "trace"), "lua1,js2,lua3");
    assert_eq!(fx.text("mixed", "fromLua"), "lua-ok");
    assert_eq!(fx.text("mixed", "fromJs"), "js-ok");

    let (req_tx, req_rx) = mpsc::channel();
    thread::spawn(move || {
        let _ = req_tx.send(exchange_get(addr, "/info.json"));
    });
    let deadline = Instant::now() + TIMEOUT;
    let info = loop {
        while let Ok(input) = rx.try_recv() {
            fx.server.handle(input);
        }
        fx.server.tick(Instant::now());
        if let Ok(reply) = req_rx.try_recv() {
            break reply.expect("info.json reply");
        }
        assert!(Instant::now() < deadline, "timed out waiting for info.json");
        thread::sleep(STEP);
    };
    assert_eq!(info.0, 200);
    let json: serde_json::Value = serde_json::from_str(&info.1).expect("valid json");
    let resources = json["resources"]
        .as_array()
        .expect("resources array")
        .iter()
        .filter(|v| v.as_str() == Some("mixed"))
        .count();
    assert_eq!(resources, 1, "mixed resource listed once in /info.json");

    fx.manager().stop("mixed").expect("stop mixed");
    assert_eq!(
        fx.manager().state_get("global", "stoppedLua"),
        Some(Value::Bool(true))
    );
    assert_eq!(
        fx.manager().state_get("global", "stoppedJs"),
        Some(Value::Bool(true))
    );
    assert!(!fx.manager().is_running("mixed"));
}

#[test]
fn js_server_and_client_scripts_sync_and_communicate_over_loopback() {
    let fx = Fixture::new();
    fx.add_resource(
        "js_prov",
        "fx_version 'cerulean'\ngame 'common'\nclient_script 'prov.js'\nexport 'greet'\n",
        &[(
            "prov.js",
            r#"
            exports("greet", (name) => `hello ${name} (${GetCurrentResourceName()})`);
            "#,
        )],
    );
    fx.add_resource(
        "js_app",
        "fx_version 'cerulean'\ngame 'common'\n\
         dependency 'js_prov'\n\
         server_script 'srv.js'\n\
         client_script 'cli.js'\n\
         server_export 'report'\n",
        &[
            (
                "srv.js",
                r#"
                const lines = [];
                GlobalState.jsReady = "initial";
                RegisterNetEvent("js:report", (msg) => {
                    lines.push(`${source}:${msg}`);
                    if (msg.startsWith("boot:")) {
                        TriggerClientEvent("js:pong", source, "welcome");
                    }
                });
                exports("report", () => lines.join("\n"));
                "#,
            ),
            (
                "cli.js",
                r#"
                const greeting = exports.js_prov.greet("alice");
                RegisterNetEvent("js:pong", (reply) => {
                    const ped = CreatePed(4, "mp_m_freemode_01", 1, 2, 3, 90, true, false);
                    TriggerServerEvent("js:report", `pong:${source}:${reply}:${DoesEntityExist(ped)}`);
                });
                TriggerServerEvent("js:report", `boot:${greeting}:${GlobalState.jsReady}`);
                "#,
            ),
        ],
    );

    fx.server.execute("endpoint_add_udp 127.0.0.1:0");
    let (tx, rx) = mpsc::channel();
    let addr = fx.server.start_network(tx).expect("network");
    fx.server.execute("ensure js_app");

    let cache = TempDir::new().expect("cache dir");
    let data = TempDir::new().expect("data dir");
    let cache_path = cache.path().to_path_buf();
    let (done_tx, done_rx) = mpsc::channel();
    thread::spawn(move || {
        let _ = done_tx.send(Client::connect(
            addr,
            "JsPlayer",
            None,
            &Identity::Ephemeral,
            cache_path,
            TIMEOUT,
        ));
    });

    let deadline = Instant::now() + TIMEOUT;
    let client = loop {
        while let Ok(input) = rx.try_recv() {
            fx.server.handle(input);
        }
        fx.server.tick(Instant::now());
        if let Ok(res) = done_rx.try_recv() {
            break res.expect("client connect");
        }
        assert!(Instant::now() < deadline, "timed out connecting client");
        thread::sleep(STEP);
    };

    let rt = ClientRuntime::new(client, data.path());
    rt.start_synced();

    let deadline = Instant::now() + TIMEOUT;
    let report = loop {
        while let Ok(input) = rx.try_recv() {
            fx.server.handle(input);
        }
        fx.server.tick(Instant::now());
        rt.pump(Duration::ZERO);
        rt.tick(Instant::now());
        let text = fx.text("js_app", "report");
        if text.contains("pong:65535:welcome:true") {
            break text;
        }
        assert!(
            Instant::now() < deadline,
            "timed out waiting for JS client report, got: {text:?}"
        );
        thread::sleep(STEP);
    };
    assert!(report.contains("boot:hello alice (js_prov):initial"));
    assert!(report.contains("pong:65535:welcome:true"));

    rt.shutdown();
}
