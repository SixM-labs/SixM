//! `PerformHttpRequest` end to end: a real server resource calls a local test
//! HTTP server, and its callback records what came back.

mod common;

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::thread;
use std::time::{Duration, Instant};

use common::{Fixture, manifest};
use sixm_core::{Host, Value};

/// Longest a test waits for something the server should do.
const TIMEOUT: Duration = Duration::from_secs(10);

/// Reads one request: its head and, by `Content-Length`, its body.
fn read_request(stream: &mut TcpStream) -> Option<(String, Vec<u8>)> {
    let mut data = Vec::new();
    let mut chunk = [0_u8; 1024];
    let split = loop {
        if let Some(at) = data.windows(4).position(|window| window == b"\r\n\r\n") {
            break at + 4;
        }
        match stream.read(&mut chunk).ok()? {
            0 => return None,
            read => data.extend_from_slice(&chunk[..read]),
        }
    };
    let head = String::from_utf8_lossy(&data[..split]).into_owned();
    let length = head
        .lines()
        .find_map(|line| {
            line.to_ascii_lowercase()
                .strip_prefix("content-length:")?
                .trim()
                .parse()
                .ok()
        })
        .unwrap_or(0_usize);
    while data.len() < split + length {
        match stream.read(&mut chunk).ok()? {
            0 => return None,
            read => data.extend_from_slice(&chunk[..read]),
        }
    }
    Some((head, data[split..split + length].to_vec()))
}

/// Serves `connections` requests on a loopback port. Each answer is `201` with
/// the body `"<method> <path> <request body> <x-test header>"`, sent after
/// `delay`. Returns the base URL.
fn serve(connections: usize, delay: Duration) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let port = listener.local_addr().expect("addr").port();
    thread::spawn(move || {
        for _ in 0..connections {
            let Ok((mut stream, _)) = listener.accept() else {
                return;
            };
            let Some((head, body)) = read_request(&mut stream) else {
                continue;
            };
            let mut first = head.lines().next().unwrap_or_default().split(' ');
            let method = first.next().unwrap_or_default();
            let path = first.next().unwrap_or_default();
            let header = head
                .lines()
                .find_map(|line| {
                    line.to_ascii_lowercase()
                        .strip_prefix("x-test:")
                        .map(|v| v.trim().to_owned())
                })
                .unwrap_or_default();
            let text = format!(
                "{method} {path} {} {header}",
                String::from_utf8_lossy(&body)
            );
            thread::sleep(delay);
            let reply = format!(
                "HTTP/1.1 201 Created\r\nContent-Type: text/plain\r\nContent-Length: {}\r\n\
                 Connection: close\r\n\r\n{text}",
                text.len()
            );
            let _ = stream.write_all(reply.as_bytes());
        }
    });
    format!("http://127.0.0.1:{port}")
}

/// Calls export `export` and returns its first result as text.
fn log(fx: &Fixture, resource: &str, export: &str) -> String {
    fx.text(resource, export)
}

/// Ticks the server until `done` holds or the timeout passes.
fn tick_until(fx: &Fixture, mut done: impl FnMut() -> bool) {
    let deadline = Instant::now() + TIMEOUT;
    while !done() {
        assert!(Instant::now() < deadline, "the condition never held");
        fx.server.tick(Instant::now());
        thread::sleep(Duration::from_millis(5));
    }
}

#[test]
fn a_resource_gets_status_body_and_headers_in_its_callback() {
    let fx = Fixture::new();
    fx.server.execute("set sixm_httpWorkers 2");
    fx.server.execute("set sv_httpAllowPrivate true");
    let base = serve(2, Duration::ZERO);
    let script = format!(
        r"
LOG = {{}}
PerformHttpRequest('{base}/post', function(status, body, headers, err)
    LOG[#LOG + 1] = status .. '|' .. body .. '|' .. headers['content-type'] .. '|' .. err
end, 'POST', 'ping', {{ ['X-Test'] = 'yes' }})
PerformHttpRequest('http://127.0.0.1:1/', function(status, body, headers, err)
    LOG[#LOG + 1] = 'failed ' .. status .. ' [' .. body .. '] ' .. tostring(err ~= '')
end, 'GET', '', {{}}, {{ timeout = 2000 }})
exports('log', function() table.sort(LOG) return table.concat(LOG, ';') end)
exports('count', function() return #LOG end)
"
    );
    fx.add_resource(
        "fetcher",
        &manifest(&["server.lua"], &["log", "count"], &[]),
        &[("server.lua", &script)],
    );
    fx.manager().start("fetcher").expect("fetcher starts");

    tick_until(&fx, || {
        matches!(
            fx.manager()
                .call_export("fetcher", "count", &[], None)
                .as_deref(),
            Ok([Value::Int(2)])
        )
    });
    assert_eq!(
        log(&fx, "fetcher", "log"),
        "201|POST /post ping yes|text/plain|;failed 0 [] true"
    );
}

#[test]
fn a_stopped_resource_gets_no_callback() {
    let fx = Fixture::new();
    fx.server.execute("set sv_httpAllowPrivate true");
    let base = serve(1, Duration::from_millis(300));
    fx.add_resource(
        "observer",
        &manifest(&["server.lua"], &["record", "log"], &[]),
        &[(
            "server.lua",
            "LOG = {}\n\
             exports('record', function(entry) LOG[#LOG + 1] = entry end)\n\
             exports('log', function() return table.concat(LOG, ' ') end)\n",
        )],
    );
    let script = format!(
        "PerformHttpRequest('{base}/slow', function(status)\n\
             exports.observer:record('called ' .. status)\n\
         end)\n"
    );
    fx.add_resource(
        "fetcher",
        &manifest(&["server.lua"], &[], &[]),
        &[("server.lua", &script)],
    );
    fx.manager().start("observer").expect("observer starts");
    fx.manager().start("fetcher").expect("fetcher starts");
    fx.manager().stop("fetcher").expect("fetcher stops");

    // Long enough for the response to arrive and be offered to a frame.
    let until = Instant::now() + Duration::from_millis(900);
    while Instant::now() < until {
        fx.server.tick(Instant::now());
        thread::sleep(Duration::from_millis(5));
    }
    assert_eq!(log(&fx, "observer", "log"), "");

    // The same resource started again works normally.
    fx.manager().start("fetcher").expect("fetcher restarts");
}

#[test]
fn private_and_loopback_http_requests_require_explicit_opt_in() {
    let fx = Fixture::new();
    let base = serve(1, Duration::ZERO);
    let script = format!(
        "OUT = ''\n\
         exports('fetch', function()\n\
             PerformHttpRequest('{base}/ok', function(status, body, _, err)\n\
                 OUT = status .. '|' .. body .. '|' .. tostring(err ~= '')\n\
             end)\n\
         end)\n\
         exports('out', function() return OUT end)\n"
    );
    fx.add_resource(
        "probe",
        &manifest(&["server.lua"], &["fetch", "out"], &[]),
        &[("server.lua", &script)],
    );
    fx.manager().start("probe").expect("probe starts");

    fx.manager()
        .call_export("probe", "fetch", &[], None)
        .expect("fetch");
    tick_until(&fx, || !log(&fx, "probe", "out").is_empty());
    assert_eq!(log(&fx, "probe", "out"), "0||true");

    fx.server.execute("set sv_httpAllowPrivate true");
    fx.manager()
        .call_export("probe", "fetch", &[], None)
        .expect("fetch after opt-in");
    tick_until(&fx, || log(&fx, "probe", "out").starts_with("201|"));
    assert_eq!(log(&fx, "probe", "out"), "201|GET /ok  |false");
}
