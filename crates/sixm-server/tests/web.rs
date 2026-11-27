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
