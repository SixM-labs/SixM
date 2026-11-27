//! Integration tests for server operations: Chrome Trace `profiler` commands
//! and authenticated `POST /rcon` (`rcon_password`).

mod common;

use std::fs;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use common::net::{Loopback, Resource};
use common::{Fixture, manifest};
use serde_json::Value as Json;
use sixm_core::{Host, Value};

const RCON_TEST_RESOURCE: Resource<'static> = Resource {
    name: "app",
    script: r"
        local changes = 0
        AddConvarChangeListener(nil, function(name)
            if name:lower() == 'rcon_password' then changes = changes + 1 end
        end)
        RegisterCommand('rcon_ping', function()
            print('pong from rcon')
            ExecuteCommand('help')
        end, false)
        exports('credentials', function()
            local changed = pcall(SetConvar, 'rcon_password', 'script-password')
            ExecuteCommand('set rcon_password queued-password')
            return GetConvar('RCON_PASSWORD', 'hidden'), changes, changed
        end)
    ",
    exports: &[],
};

struct RemoveOnDrop(String);

impl Drop for RemoveOnDrop {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
    }
}

fn post_raw(addr: SocketAddr, extra_headers: &str, body: &str) -> (u16, Vec<u8>) {
    let mut stream = TcpStream::connect(addr).expect("tcp connect");
    stream
        .set_read_timeout(Some(Duration::from_secs(10)))
        .expect("timeout");
    let req = format!(
        "POST /rcon HTTP/1.1\r\nHost: localhost\r\nContent-Type: application/json\r\n{extra_headers}Content-Length: {}\r\n\r\n{body}",
        body.len()
    );
    stream.write_all(req.as_bytes()).expect("write request");
    let mut data = Vec::new();
    stream.read_to_end(&mut data).expect("read response");
    let split = data
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .expect("head")
        + 4;
    let head = String::from_utf8_lossy(&data[..split]);
    let status = head
        .split_whitespace()
        .nth(1)
        .expect("status")
        .parse()
        .expect("u16 status");
    (status, data[split..].to_vec())
}

fn send_rcon(lb: &Loopback, extra_headers: &str, body: &str) -> (u16, Vec<u8>) {
    let (tx, rx) = mpsc::channel();
    let addr = lb.addr;
    let extra = extra_headers.to_owned();
    let payload = body.to_owned();
    thread::spawn(move || {
        let _ = tx.send(post_raw(addr, &extra, &payload));
    });
    lb.wait_for("rcon HTTP response", || rx.try_recv().ok())
}

#[test]
fn profiler_records_resources_and_saves_chrome_trace_json() {
    let fx = Fixture::new();
    fx.add_resource(
        "prof_res",
        &manifest(&["server.lua"], &[], &[]),
        &[(
            "server.lua",
            "CreateThread(function() while true do Wait(0) end end)\n",
        )],
    );
    fx.server.execute("ensure prof_res");

    fx.server.execute("profiler record 3");
    for _ in 0..4 {
        fx.server.tick(Instant::now());
    }

    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    let rel = format!("target/sixm_test_profile_{nanos}.json");
    let _cleanup = RemoveOnDrop(rel.clone());

    fx.server.execute("profiler saveJSON ../escape.json");
    fx.server.execute(&format!("profiler saveJSON {rel}"));

    let bytes = fs::read(&rel).expect("saved profiler json");
    let doc: Json = serde_json::from_slice(&bytes).expect("valid trace json");
    let events = doc["traceEvents"].as_array().expect("traceEvents array");
    assert!(
        events
            .iter()
            .any(|e| e["name"] == "prof_res" && e["cat"] == "resource" && e["ph"] == "X"),
        "expected resource span in {doc}"
    );
    let ticks = events
        .iter()
        .filter(|e| e["name"] == "ServerTick" && e["cat"] == "frame")
        .count();
    assert_eq!(ticks, 3, "expected auto-stop after 3 frames: {doc}");
}

#[test]
fn rcon_enforces_password_rate_limit_and_returns_command_output() {
    let lb = Loopback::new(&[RCON_TEST_RESOURCE]);

    // Disabled by default when rcon_password is unset/empty.
    let (status, _) = send_rcon(&lb, "", r#"{"password":"secret","command":"help"}"#);
    assert_eq!(status, 403);

    lb.fx.server.execute("set rcon_password super-secret-pw");

    // 5 wrong attempts trigger rate lockout (`429`) on the 6th.
    for _ in 0..5 {
        let (st, _) = send_rcon(&lb, "", r#"{"password":"wrong","command":"help"}"#);
        assert_eq!(st, 401);
    }
    let (locked, _) = send_rcon(
        &lb,
        "",
        r#"{"password":"super-secret-pw","command":"help"}"#,
    );
    assert_eq!(locked, 429);

    // Re-test success on a fresh server so the test finishes in milliseconds.
    let lb2 = Loopback::new(&[RCON_TEST_RESOURCE]);
    lb2.fx
        .server
        .execute("add_ace resource.app command.set allow");
    lb2.fx
        .server
        .execute("add_ace resource.app command.help allow");
    lb2.fx.server.execute("set RCON_PASSWORD super-secret-pw");
    let credentials = lb2
        .fx
        .manager()
        .call_export("app", "credentials", &[], None)
        .expect("read credentials export");
    assert_eq!(
        credentials,
        [Value::from("hidden"), Value::Int(0), Value::Bool(false)]
    );
    lb2.pump();
    lb2.fx.server.execute("sets rcon_password public-password");
    lb2.fx
        .server
        .execute("setr rcon_password replicated-password");

    let (ok_status, body) = send_rcon(
        &lb2,
        "Authorization: Bearer super-secret-pw\r\n",
        r#"{"command":"rcon_ping"}"#,
    );
    assert_eq!(ok_status, 200);
    let doc: Json = serde_json::from_slice(&body).expect("rcon json");
    assert_eq!(doc["ok"], true);
    let out = doc["output"].as_array().expect("output array");
    assert!(
        out.iter()
            .any(|line| line.as_str().is_some_and(|s| s.contains("pong from rcon"))),
        "expected command output in {doc}"
    );
    assert!(out.iter().any(|line| {
        line.as_str()
            .is_some_and(|s| s.contains("Built-in commands"))
    }));

    lb2.fx.server.execute("set rcon_password \"\"");
    let (disabled, _) = send_rcon(&lb2, "X-Rcon-Password: super-secret-pw\r\n", "help");
    assert_eq!(disabled, 403);
}
