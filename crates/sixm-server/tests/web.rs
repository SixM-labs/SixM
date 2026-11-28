//! The HTTP listener end to end: built-in routes, `SetHttpHandler` handlers and
//! the limits, over loopback TCP on the game port.

mod common;

use std::io::{self, Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use common::net::{Loopback, Resource};

/// A parsed answer: the status, the head and the body.
#[derive(Debug)]
struct Reply {
    status: u16,
    head: String,
    body: Vec<u8>,
}

impl Reply {
    fn text(&self) -> String {
        String::from_utf8_lossy(&self.body).into_owned()
    }
}

/// The script of the test resource `app`: a route that completes, one that
/// echoes a body through a body handler, and one that fails.
const APP: &str = "LOG = {}\n\
  function log() return table.concat(LOG, ' ') end\n\
  SetHttpHandler(function(req, res)\n\
    if req.path == '/app/hello' then\n\
      res.writeHead(201, { ['Content-Type'] = 'text/plain', ['X-Query'] = req.query })\n\
      res.send('hello ' .. req.method)\n\
    elseif req.path == '/app/hang' then\n\
      LOG[#LOG + 1] = 'hang'\n\
    elseif req.path == '/app/echo' then\n\
      req.setDataHandler(function(data) res:send('echo:' .. data) end)\n\
    else\n\
      error('no such route')\n\
    end\n\
  end)\n";

/// The script of the test resource `other`: it answers every request it gets.
const OTHER: &str = "SetHttpHandler(function(req, res) res.send('other') end)\n";

/// Sends `raw` over a new connection and reads the answer until the server closes it.
fn exchange(addr: SocketAddr, raw: &[u8]) -> io::Result<Reply> {
    let mut stream = TcpStream::connect(addr)?;
    stream.set_read_timeout(Some(Duration::from_secs(10)))?;
    stream.write_all(raw)?;
    let mut data = Vec::new();
    stream.read_to_end(&mut data)?;
    let split = data
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "missing HTTP response head"))?
        + 4;
    let head = String::from_utf8_lossy(&data[..split]).into_owned();
    let status = head
        .split_whitespace()
        .nth(1)
        .and_then(|status| status.parse().ok())
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "invalid HTTP status"))?;
    Ok(Reply {
        status,
        head,
        body: data[split..].to_vec(),
    })
}

/// Makes one request while the test thread pumps the server, and returns the answer.
fn send(lb: &Loopback, raw: Vec<u8>) -> Reply {
    let (sender, answers) = mpsc::channel();
    let addr = lb.addr;
    thread::spawn(move || {
        let _ = sender.send(exchange(addr, &raw));
    });
    lb.wait_for("an HTTP answer", || answers.try_recv().ok())
        .expect("the connection was answered")
}

fn get(lb: &Loopback, path: &str) -> Reply {
    send(
        lb,
        format!("GET {path} HTTP/1.1\r\nHost: localhost\r\n\r\n").into_bytes(),
    )
}

fn post(lb: &Loopback, path: &str, body: &str) -> Reply {
    send(
        lb,
        format!(
            "POST {path} HTTP/1.1\r\nHost: localhost\r\nContent-Length: {}\r\n\r\n{body}",
            body.len()
        )
        .into_bytes(),
    )
}

fn started() -> Loopback {
    Loopback::new(&[
        Resource {
            name: "app",
            script: APP,
            exports: &["log"],
        },
        Resource {
            name: "other",
            script: OTHER,
            exports: &[],
        },
    ])
}

#[test]
fn info_json_lists_the_running_resources() {
    let lb = started();
    lb.fx.server.execute("sets sv_projectName demo");

    let reply = get(&lb, "/info.json");

    assert_eq!(reply.status, 200);
    let document: serde_json::Value = serde_json::from_slice(&reply.body).expect("JSON");
    assert_eq!(document["resources"], serde_json::json!(["app", "other"]));
    assert_eq!(document["vars"]["sv_projectname"], "demo");
}

#[test]
fn dynamic_json_reports_the_name_and_the_player_limit() {
    let lb = started();

    let reply = get(&lb, "/dynamic.json");

    assert_eq!(reply.status, 200);
    let document: serde_json::Value = serde_json::from_slice(&reply.body).expect("JSON");
    assert_eq!(document["hostname"], "SixM Server");
    assert_eq!(document["clients"], 0);
    assert_eq!(document["maxclients"], 32);
}

#[test]
fn players_json_is_empty_without_players() {
    let lb = started();

    let reply = get(&lb, "/players.json");

    assert_eq!(reply.status, 200);
    assert_eq!(reply.text(), "[]");
}

#[test]
fn built_in_routes_only_accept_get() {
    let lb = started();

    let reply = post(&lb, "/info.json", "x");

    assert_eq!(reply.status, 405);
    assert!(reply.head.to_ascii_lowercase().contains("allow: get, head"));
}

#[test]
fn an_unknown_path_is_not_found() {
    let lb = started();

    assert_eq!(get(&lb, "/nothing/here").status, 404);
}

#[test]
fn a_resource_handler_answers_with_its_head_and_body() {
    let lb = started();

    let reply = get(&lb, "/app/hello?name=x");

    assert_eq!(reply.status, 201);
    assert!(reply.head.to_ascii_lowercase().contains("x-query: name=x"));
    assert_eq!(reply.text(), "hello GET");
}

#[test]
fn a_body_handler_receives_the_request_body() {
    let lb = started();

    let reply = post(&lb, "/app/echo", "payload");

    assert_eq!(reply.status, 200);
    assert_eq!(reply.text(), "echo:payload");
}

#[test]
fn a_handler_that_fails_gives_a_server_error() {
    let lb = started();

    assert_eq!(get(&lb, "/app/broken").status, 500);
}

#[test]
fn an_oversized_body_is_refused_before_it_is_read() {
    let lb = started();
    let raw =
        b"POST /app/echo HTTP/1.1\r\nHost: localhost\r\nContent-Length: 2000000\r\n\r\n".to_vec();

    assert_eq!(send(&lb, raw).status, 413);
}

#[test]
fn a_client_sending_too_often_is_slowed_down() {
    let lb = started();

    let refused = (0..80)
        .filter(|_| get(&lb, "/dynamic.json").status == 429)
        .count();

    assert!(refused > 0, "no request was refused");
}

#[test]
fn the_handler_goes_when_its_resource_stops() {
    let lb = started();
    assert_eq!(get(&lb, "/app/hello").status, 201);

    lb.fx.server.execute("stop app");

    assert_eq!(get(&lb, "/app/hello").status, 404);
}

#[test]
fn a_handler_that_never_answers_gets_a_gateway_timeout() {
    let lb = started();
    lb.fx.server.execute("set sixm_httpHandlerTimeout 1");

    let begun = std::time::Instant::now();
    let reply = get(&lb, "/app/hang");

    assert_eq!(reply.status, 504);
    assert!(begun.elapsed() >= Duration::from_secs(1));
}

#[test]
fn requests_a_stopped_resource_has_not_answered_get_503() {
    let lb = started();
    let (sender, answers) = mpsc::channel();
    let addr = lb.addr;
    thread::spawn(move || {
        let _ = sender.send(exchange(
            addr,
            b"GET /app/hang HTTP/1.1\r\nHost: localhost\r\n\r\n",
        ));
    });
    lb.wait_for_log("hang");

    lb.fx.server.execute("stop app");

    let reply = lb
        .wait_for("the answer", || answers.try_recv().ok())
        .expect("the connection was answered");
    assert_eq!(reply.status, 503);
}

#[test]
fn a_path_never_reaches_another_resources_handler() {
    let lb = started();
    assert_eq!(get(&lb, "/other/x").text(), "other");

    for path in [
        "/app%2F..%2Fother/x",
        "/app%2Fother/x",
        "/%6Fther/x",
        "/app/../other/x",
        "/app/%2e%2e/other/x",
        "//other/x",
        "/./other/x",
        "/app/..%2Fother/x",
    ] {
        let reply = get(&lb, path);
        assert_ne!(reply.text(), "other", "{path} reached `other`");
    }
}

#[test]
fn a_handler_sees_the_path_as_sent() {
    let lb = started();

    // The handler of `app` gets the raw path and answers 500: it has no such route.
    assert_eq!(get(&lb, "/app/../other/x").status, 500);
    assert_eq!(get(&lb, "/app%2Fhello").status, 404);
}

#[test]
fn players_json_hides_identifiers_unless_they_are_exposed() {
    let lb = started();
    let _client = lb.connect("Alice").expect("Alice is admitted");

    let hidden: serde_json::Value =
        serde_json::from_slice(&get(&lb, "/players.json").body).expect("JSON");
    assert_eq!(hidden[0]["name"], "Alice");
    assert!(hidden[0].get("identifiers").is_none(), "{hidden}");

    lb.fx.server.execute("set sv_exposePlayerIdentifiers true");
    let shown: serde_json::Value =
        serde_json::from_slice(&get(&lb, "/players.json").body).expect("JSON");
    assert!(
        shown[0]["identifiers"]
            .as_array()
            .is_some_and(|ids| !ids.is_empty()),
        "{shown}"
    );
}

#[test]
fn http_can_be_switched_off() {
    let fx = common::Fixture::new();
    fx.server.execute("endpoint_add_udp 127.0.0.1:0");
    fx.server.execute("set sv_httpEnabled false");
    let (sender, _input) = mpsc::channel();
    let addr = fx.server.start_network(sender).expect("the network starts");

    assert!(TcpStream::connect(addr).is_err(), "nothing listens on TCP");
}

#[test]
fn a_busy_tcp_port_leaves_the_game_running_without_http() {
    let blocker = std::net::TcpListener::bind("127.0.0.1:0").expect("a free port");
    let port = blocker.local_addr().expect("address").port();
    let fx = common::Fixture::new();
    fx.server
        .execute(&format!("endpoint_add_udp 127.0.0.1:{port}"));
    let (sender, _input) = mpsc::channel();

    let addr = fx
        .server
        .start_network(sender)
        .expect("the game starts although TCP is taken");

    assert_eq!(addr.port(), port);
}

#[test]
fn healthz_returns_ok_with_resource_and_player_counts() {
    let lb = started();

    let reply = get(&lb, "/healthz");
    assert_eq!(reply.status, 200);
    let doc: serde_json::Value = serde_json::from_slice(&reply.body).expect("JSON");
    assert_eq!(
        doc,
        serde_json::json!({ "ok": true, "resources": 2, "players": 0 })
    );

    let _alice = lb.connect("Alice").expect("Alice joins");
    let reply = get(&lb, "/healthz");
    assert_eq!(reply.status, 200);
    let doc: serde_json::Value = serde_json::from_slice(&reply.body).expect("JSON");
    assert_eq!(
        doc,
        serde_json::json!({ "ok": true, "resources": 2, "players": 1 })
    );

    assert_eq!(post(&lb, "/healthz", "x").status, 405);
}

#[test]
fn metrics_is_gated_by_sv_metrics_enabled_and_exposes_prometheus_format() {
    let lb = started();
    assert_eq!(get(&lb, "/metrics").status, 404);

    lb.fx.server.execute("set sv_metricsEnabled true");
    let reply = get(&lb, "/metrics");
    assert_eq!(reply.status, 200);
    assert!(
        reply
            .head
            .to_ascii_lowercase()
            .contains("content-type: text/plain; version=0.0.4; charset=utf-8"),
        "head: {}",
        reply.head
    );
    let text = reply.text();
    for expected in [
        "sixm_players_connected 0",
        "sixm_players_max 32",
        "sixm_resources_running 2",
        "sixm_entities_total 0",
        "sixm_tick_duration_ms{quantile=\"avg\"}",
        "sixm_tick_duration_ms{quantile=\"p50\"}",
        "sixm_tick_duration_ms{quantile=\"p99\"}",
        "sixm_tick_duration_ms{quantile=\"max\"}",
        "sixm_net_rx_bytes_total ",
        "sixm_net_tx_bytes_total ",
    ] {
        assert!(text.contains(expected), "missing `{expected}` in:\n{text}");
    }
    assert_eq!(post(&lb, "/metrics", "x").status, 405);
}

#[test]
fn admin_ui_requires_both_sv_admin_ui_enabled_and_rcon_password() {
    let lb = started();
    assert_eq!(get(&lb, "/admin").status, 404);

    lb.fx.server.execute("set sv_adminUiEnabled 1");
    assert_eq!(get(&lb, "/admin").status, 403);

    lb.fx.server.execute("set rcon_password admin-pw");
    let reply = get(&lb, "/admin");
    assert_eq!(reply.status, 200);
    assert!(
        reply
            .head
            .to_ascii_lowercase()
            .contains("content-type: text/html; charset=utf-8"),
        "head: {}",
        reply.head
    );
    let html = reply.text();
    assert!(html.contains("<title>SixM Server Admin</title>"));
    assert!(html.contains("/rcon"));
    assert!(!html.contains("http://") && !html.contains("https://"));
    assert_eq!(post(&lb, "/admin", "x").status, 405);

    lb.fx.server.execute("set rcon_password \"\"");
    assert_eq!(get(&lb, "/admin").status, 403);
}

#[test]
fn resmon_json_is_gated_and_reports_running_resources() {
    let lb = started();
    assert_eq!(get(&lb, "/resmon.json").status, 404);

    lb.fx.server.execute("set sv_adminUiEnabled 1");
    assert_eq!(get(&lb, "/resmon.json").status, 403);

    lb.fx.server.execute("set rcon_password admin-pw");
    lb.fx.server.tick(std::time::Instant::now());

    let admin_html = get(&lb, "/admin").text();
    assert!(admin_html.contains("/resmon.json"));
    assert!(admin_html.contains("Resource Monitor (resmon)"));

    let reply = get(&lb, "/resmon.json");
    assert_eq!(reply.status, 200);
    assert_eq!(post(&lb, "/resmon.json", "x").status, 405);

    let doc: serde_json::Value = serde_json::from_slice(&reply.body).expect("JSON");
    assert_eq!(doc["hitch_warning_ms"], 50);
    let resources = doc["resources"].as_array().expect("resources array");
    let app = resources
        .iter()
        .find(|r| r["name"] == "app")
        .expect("app resource in resmon");
    assert!(app["samples"].as_u64().unwrap_or(0) >= 1);
    assert!(app["heap_bytes"].as_u64().unwrap_or(0) > 0);
}

#[test]
fn master_heartbeat_posts_info_and_dynamic_to_configured_url() {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind mock master");
    let port = listener.local_addr().expect("addr").port();
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        if let Ok((mut stream, _)) = listener.accept() {
            let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
            let mut buf = Vec::new();
            let mut chunk = [0_u8; 1024];
            let mut expected_len: Option<usize> = None;
            loop {
                match stream.read(&mut chunk) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        buf.extend_from_slice(&chunk[..n]);
                        if let Some(split) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                            let head_end = split + 4;
                            if expected_len.is_none() {
                                let head = String::from_utf8_lossy(&buf[..split]);
                                expected_len = head.lines().find_map(|line| {
                                    let (k, v) = line.split_once(':')?;
                                    k.eq_ignore_ascii_case("content-length")
                                        .then(|| v.trim().parse().ok())
                                        .flatten()
                                });
                            }
                            if let Some(len) = expected_len
                                && buf.len() >= head_end + len
                            {
                                break;
                            }
                        }
                    }
                }
            }
            let _ = stream
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok");
            let _ = tx.send(buf);
        }
    });

    let lb = started();
    lb.fx.server.execute("set sv_httpAllowPrivate true");
    lb.fx
        .server
        .execute(&format!("set sv_masterUrl \"http://127.0.0.1:{port}/hb\""));

    let raw = lb.wait_for("master heartbeat request", || {
        thread::sleep(Duration::from_millis(5));
        rx.try_recv().ok()
    });
    let split = raw
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .expect("HTTP head")
        + 4;
    let head = String::from_utf8_lossy(&raw[..split]);
    assert!(head.starts_with("POST /hb "), "head: {head}");
    let body: serde_json::Value = serde_json::from_slice(&raw[split..]).expect("JSON body");
    assert_eq!(
        body["info"]["resources"],
        serde_json::json!(["app", "other"])
    );
    assert_eq!(body["dynamic"]["hostname"], "SixM Server");
}

#[test]
fn proxy_ip_ranges_governs_forwarded_rate_limits_and_rcon_attribution() {
    let lb = started();

    // Without sv_proxyIPRanges, X-Forwarded-For from 127.0.0.1 is ignored and rate limit is shared.
    let mut saw_429 = false;
    for i in 1..=80 {
        let req = format!(
            "GET /dynamic.json HTTP/1.1\r\nHost: localhost\r\nX-Forwarded-For: 203.0.113.{i}\r\n\r\n"
        );
        if send(&lb, req.into_bytes()).status == 429 {
            saw_429 = true;
            break;
        }
    }
    assert!(saw_429, "spoofed XFF should not bypass peer rate limit");

    // Trust loopback proxy on a fresh listener: rate limits and RCON lockout isolate by forwarded IP.
    let lb2 = started();
    lb2.fx
        .server
        .execute("set sv_proxyIPRanges \"127.0.0.1/32\"");
    lb2.fx.server.execute("set rcon_password super-secret-pw");

    let mut exhausted = false;
    for _ in 0..80 {
        let req = b"GET /dynamic.json HTTP/1.1\r\nHost: localhost\r\nX-Forwarded-For: 203.0.113.10\r\n\r\n".to_vec();
        if send(&lb2, req).status == 429 {
            exhausted = true;
            break;
        }
    }
    assert!(exhausted, "203.0.113.10 should hit its own rate limit");
    let other_client = send(
        &lb2,
        b"GET /dynamic.json HTTP/1.1\r\nHost: localhost\r\nX-Real-IP: 203.0.113.11\r\n\r\n"
            .to_vec(),
    );
    assert_eq!(other_client.status, 200);

    let rcon_req = |xff: &str, body: &str| {
        format!(
            "POST /rcon HTTP/1.1\r\nHost: localhost\r\nX-Forwarded-For: {xff}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
            body.len()
        )
        .into_bytes()
    };

    // 5 failed RCON attempts lock out forwarded 198.51.100.10 without blocking 198.51.100.11.
    for _ in 0..5 {
        let bad = send(
            &lb2,
            rcon_req("198.51.100.10", r#"{"password":"wrong","command":"perf"}"#),
        );
        assert_eq!(bad.status, 401);
    }
    let locked = send(
        &lb2,
        rcon_req(
            "198.51.100.10",
            r#"{"password":"super-secret-pw","command":"perf"}"#,
        ),
    );
    assert_eq!(locked.status, 429);

    let isolated = send(
        &lb2,
        rcon_req(
            "198.51.100.11",
            r#"{"password":"super-secret-pw","command":"perf"}"#,
        ),
    );
    assert_eq!(isolated.status, 200);
}

#[test]
fn metrics_enforces_basic_auth_and_hides_password_from_scripts() {
    let lb = started();
    lb.fx.server.execute("set sv_metricsEnabled true");

    // Half-configured user without password fails closed (`401`).
    lb.fx.server.execute("set sv_prometheusBasicAuthUser prom");
    let half = get(&lb, "/metrics");
    assert_eq!(half.status, 401);
    assert!(
        half.head
            .to_ascii_lowercase()
            .contains("www-authenticate: basic realm=\"sixm metrics\""),
        "head: {}",
        half.head
    );

    lb.fx
        .server
        .execute("set sv_prometheusBasicAuthPassword prom-pw");
    assert_eq!(get(&lb, "/metrics").status, 401);

    let wrong = send(
        &lb,
        b"GET /metrics HTTP/1.1\r\nHost: localhost\r\nAuthorization: Basic YmFkOmJhZA==\r\n\r\n"
            .to_vec(),
    );
    assert_eq!(wrong.status, 401);

    // "prom:prom-pw" in base64 is "cHJvbTpwcm9tLXB3".
    let ok = send(
        &lb,
        b"GET /metrics HTTP/1.1\r\nHost: localhost\r\nAuthorization: Basic cHJvbTpwcm9tLXB3\r\n\r\n"
            .to_vec(),
    );
    assert_eq!(ok.status, 200);
    assert!(ok.text().contains("sixm_players_connected"));

    // Script cannot inspect or alter console-only Prometheus Basic Auth convars.
    assert_eq!(
        lb.fx
            .server
            .manager()
            .convar("sv_prometheusBasicAuthPassword"),
        None
    );
}

#[test]
fn lua_and_js_handlers_read_direct_and_forwarded_address() {
    let lb = started();
    lb.fx.add_resource(
        "addr_lua",
        &common::manifest(&["server.lua"], &[], &[]),
        &[(
            "server.lua",
            "SetHttpHandler(function(req, res) res.send(req.address) end)\n",
        )],
    );
    lb.fx.add_resource(
        "addr_js",
        &common::manifest(&["server.js"], &[], &[]),
        &[(
            "server.js",
            "SetHttpHandler((req, res) => { res.send(req.address); });\n",
        )],
    );
    lb.fx.server.execute("refresh");
    lb.fx.server.execute("ensure addr_lua");
    lb.fx.server.execute("ensure addr_js");

    // Without sv_proxyIPRanges, X-Forwarded-For is ignored and peer IP is reported.
    let direct_lua = send(
        &lb,
        b"GET /addr_lua/ip HTTP/1.1\r\nHost: localhost\r\nX-Forwarded-For: 203.0.113.55\r\n\r\n"
            .to_vec(),
    );
    assert_eq!(direct_lua.status, 200);
    assert_eq!(direct_lua.text(), "127.0.0.1");

    let direct_js = send(
        &lb,
        b"GET /addr_js/ip HTTP/1.1\r\nHost: localhost\r\nX-Forwarded-For: 203.0.113.55\r\n\r\n"
            .to_vec(),
    );
    assert_eq!(direct_js.status, 200);
    assert_eq!(direct_js.text(), "127.0.0.1");

    // With loopback trusted, both Lua and JS see the forwarded client IP.
    lb.fx
        .server
        .execute("set sv_proxyIPRanges \"127.0.0.1/32\"");

    let proxied_lua = send(
        &lb,
        b"GET /addr_lua/ip HTTP/1.1\r\nHost: localhost\r\nX-Forwarded-For: 203.0.113.55\r\n\r\n"
            .to_vec(),
    );
    assert_eq!(proxied_lua.status, 200);
    assert_eq!(proxied_lua.text(), "203.0.113.55");

    let proxied_js = send(
        &lb,
        b"GET /addr_js/ip HTTP/1.1\r\nHost: localhost\r\nX-Forwarded-For: 203.0.113.55\r\n\r\n"
            .to_vec(),
    );
    assert_eq!(proxied_js.status, 200);
    assert_eq!(proxied_js.text(), "203.0.113.55");
}

#[test]
fn timeout_and_early_disconnect_fire_set_cancel_handler_in_lua_and_js() {
    let lb = started();
    lb.fx.add_resource(
        "cancel_lua",
        &common::manifest(&["server.lua"], &["log"], &[]),
        &[(
            "server.lua",
            "LOG = {}\n\
             function log() return table.concat(LOG, ',') end\n\
             SetHttpHandler(function(req, res)\n\
               req.setCancelHandler(function() LOG[#LOG + 1] = 'canceled:' .. req.path end)\n\
               if req.path == '/cancel_lua/ok' then\n\
                 res.send('ok')\n\
               else\n\
                 LOG[#LOG + 1] = 'open:' .. req.path\n\
               end\n\
             end)\n",
        )],
    );
    lb.fx.add_resource(
        "cancel_js",
        &common::manifest(&["server.js"], &["log"], &[]),
        &[(
            "server.js",
            r#"
            const LOG = [];
            exports("log", () => LOG.join(","));
            SetHttpHandler((req, res) => {
                req.setCancelHandler(() => LOG.push(`canceled:${req.path}`));
                if (req.path === "/cancel_js/ok") {
                    res.send("ok");
                } else {
                    LOG.push(`open:${req.path}`);
                }
            });
            "#,
        )],
    );
    lb.fx.server.execute("refresh");
    lb.fx.server.execute("ensure cancel_lua");
    lb.fx.server.execute("ensure cancel_js");
    lb.fx.server.execute("set sixm_httpHandlerTimeout 1");

    // Answered requests never invoke their cancel handler.
    assert_eq!(get(&lb, "/cancel_lua/ok").status, 200);
    assert_eq!(get(&lb, "/cancel_js/ok").status, 200);
    assert_eq!(lb.fx.text("cancel_lua", "log"), "");
    assert_eq!(lb.fx.text("cancel_js", "log"), "");

    // Early client TCP disconnect fires setCancelHandler promptly (well before the 1s timeout).
    for (resource, path) in [
        ("cancel_lua", "/cancel_lua/drop"),
        ("cancel_js", "/cancel_js/drop"),
    ] {
        let mut stream = TcpStream::connect(lb.addr).expect("connect");
        stream
            .write_all(format!("GET {path} HTTP/1.1\r\nHost: localhost\r\n\r\n").as_bytes())
            .expect("write");
        lb.wait_for(&format!("{resource} open"), || {
            lb.fx
                .text(resource, "log")
                .contains(&format!("open:{path}"))
                .then_some(())
        });
        drop(stream);
        lb.wait_for(&format!("{resource} disconnect cancel"), || {
            lb.fx
                .text(resource, "log")
                .contains(&format!("canceled:{path}"))
                .then_some(())
        });
    }

    // Timeout cancellation: 504 returns AND setCancelHandler runs for both Lua and JS.
    assert_eq!(get(&lb, "/cancel_lua/timeout").status, 504);
    lb.wait_for("cancel_lua timeout cancel", || {
        lb.fx
            .text("cancel_lua", "log")
            .contains("canceled:/cancel_lua/timeout")
            .then_some(())
    });

    assert_eq!(get(&lb, "/cancel_js/timeout").status, 504);
    lb.wait_for("cancel_js timeout cancel", || {
        lb.fx
            .text("cancel_js", "log")
            .contains("canceled:/cancel_js/timeout")
            .then_some(())
    });
}
