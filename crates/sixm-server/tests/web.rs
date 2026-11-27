//! The HTTP listener end to end: built-in routes, `SetHttpHandler` handlers and
//! the limits, over loopback TCP on the game port.

mod common;

use std::io::{Read, Write};
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
fn exchange(addr: SocketAddr, raw: &[u8]) -> Option<Reply> {
    let mut stream = TcpStream::connect(addr).ok()?;
    stream
        .set_read_timeout(Some(Duration::from_secs(10)))
        .ok()?;
    stream.write_all(raw).ok()?;
    let mut data = Vec::new();
    stream.read_to_end(&mut data).ok()?;
    let split = data.windows(4).position(|window| window == b"\r\n\r\n")? + 4;
    let head = String::from_utf8_lossy(&data[..split]).into_owned();
    let status = head.split_whitespace().nth(1)?.parse().ok()?;
    Some(Reply {
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
