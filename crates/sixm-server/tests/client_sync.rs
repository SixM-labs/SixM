//! Resource files end to end: a server on loopback with resources started
//! through its console, and a real `sixm_client::Client` that syncs them.
//!
//! The test thread plays the server loop. It handles the server's inputs while
//! the client waits, because the handshake needs the server to accept it.

mod common;

use std::fs;
use std::net::SocketAddr;
use std::path::Path;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::thread;
use std::time::{Duration, Instant};

use common::Fixture;
use sixm_client::{Client, ClientNotice, SyncStats};
use sixm_net::Identity;
use sixm_resources::ClientFileKind;
use sixm_server::Input;

/// Longest a test waits for the server or the client to do something.
const TIMEOUT: Duration = Duration::from_secs(20);
/// How long each wait step blocks before checking again.
const STEP: Duration = Duration::from_millis(5);

/// A server listening on loopback. Its inputs are handled by the test thread.
struct Harness {
    fx: Fixture,
    input: Receiver<Input>,
    addr: SocketAddr,
}

impl Harness {
    fn new() -> Self {
        let fx = Fixture::new();
        fx.server.execute("endpoint_add_udp 127.0.0.1:0");
        let (sender, input) = mpsc::channel();
        let addr = fx
            .server
            .start_network(sender)
            .expect("the network starts on loopback");
        Self { fx, input, addr }
    }

    /// Handles every queued network input.
    fn pump(&self) {
        while let Ok(input) = self.input.try_recv() {
            self.fx.server.handle(input);
        }
    }

    /// Connects a client that keeps its files in `cache`. The server is pumped
    /// until the client has connected and synced.
    fn connect(&self, cache: &Path) -> Client {
        let addr = self.addr;
        let cache = cache.to_path_buf();
        let handle = thread::spawn(move || {
            Client::connect(addr, "tester", None, &Identity::Ephemeral, cache, TIMEOUT)
        });
        let deadline = Instant::now() + TIMEOUT;
        while !handle.is_finished() {
            assert!(
                Instant::now() < deadline,
                "the client did not connect in time"
            );
            match self.input.recv_timeout(STEP) {
                Ok(input) => self.fx.server.handle(input),
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => break,
            }
        }
        handle
            .join()
            .expect("the client thread does not panic")
            .expect("the client connects and syncs")
    }

    /// Polls `client` until a notice satisfies `wanted`, and returns it.
    fn expect_notice(
        &self,
        client: &mut Client,
        what: &str,
        wanted: impl Fn(&ClientNotice) -> bool,
    ) -> ClientNotice {
        let deadline = Instant::now() + TIMEOUT;
        loop {
            self.pump();
            if let Some(notice) = client.poll(STEP)
                && wanted(&notice)
            {
                return notice;
            }
            assert!(Instant::now() < deadline, "timed out waiting for {what}");
        }
    }
}

/// A `fxmanifest.lua` declaring client scripts, shared scripts and files.
fn client_manifest(shared: &[&str], client: &[&str], files: &[&str]) -> String {
    let list = |items: &[&str]| {
        items
            .iter()
            .map(|item| format!("'{item}'"))
            .collect::<Vec<_>>()
            .join(", ")
    };
    format!(
        "fx_version 'cerulean'\ngame 'common'\nshared_scripts {{ {} }}\nclient_scripts {{ {} }}\nfiles {{ {} }}\n",
        list(shared),
        list(client),
        list(files)
    )
}

/// Adds the resources of the example: `hello`, `shared` and `bare`.
fn add_example_resources(fx: &Fixture) {
    fx.add_resource(
        "hello",
        &client_manifest(&[], &["client/*.lua"], &["data/greeting.txt"]),
        &[
            ("client/main.lua", "print('hello from the client side')"),
            ("data/greeting.txt", "Hello from SixM"),
        ],
    );
    fx.add_resource(
        "shared",
        &client_manifest(&["config.lua"], &["client/extra.lua"], &[]),
        &[
            ("config.lua", "Config = { debug = false }"),
            ("client/extra.lua", "print('extra')"),
        ],
    );
    fx.add_resource("bare", &client_manifest(&[], &[], &[]), &[]);
}

fn read(path: &Path) -> String {
    fs::read_to_string(path).expect("synced file is readable")
}

#[test]
fn example_like_resources_sync_in_start_order_and_a_second_connect_downloads_nothing() {
    let h = Harness::new();
    add_example_resources(&h.fx);
    h.fx.server.execute("ensure hello");
    h.fx.server.execute("ensure shared");
    h.fx.server.execute("ensure bare");
    let cache = h.fx.root().join("cache");

    let client = h.connect(&cache);
    let names: Vec<&str> = client.resources().iter().map(|r| r.name.as_str()).collect();
    assert_eq!(names, ["hello", "shared", "bare"]);
    assert_eq!(
        client.stats(),
        SyncStats {
            downloaded: 4,
            cached: 0
        }
    );

    let hello = &client.resources()[0];
    assert_eq!(hello.files[0].name, "client/main.lua");
    assert_eq!(hello.files[0].kind, ClientFileKind::ClientScript);
    assert_eq!(hello.files[1].name, "data/greeting.txt");
    assert_eq!(hello.files[1].kind, ClientFileKind::File);
    assert_eq!(read(&hello.files[1].path), "Hello from SixM");
    let shared = &client.resources()[1];
    assert_eq!(shared.files[0].kind, ClientFileKind::SharedScript);
    assert_eq!(read(&shared.files[0].path), "Config = { debug = false }");
    assert_eq!(client.resources()[2].files.len(), 0);
    drop(client);

    let again = h.connect(&cache);
    assert_eq!(
        again.stats(),
        SyncStats {
            downloaded: 0,
            cached: 4
        }
    );
}

#[test]
fn restarted_resource_with_a_changed_file_is_synced_under_the_new_hash() {
    let h = Harness::new();
    add_example_resources(&h.fx);
    h.fx.server.execute("ensure hello");
    let cache = h.fx.root().join("cache");
    let mut client = h.connect(&cache);
    let before = client.resources()[0].files[0].path.clone();

    let edited = "print('edited, and longer than before')";
    fs::write(
        h.fx.resources()
            .join("hello")
            .join("client")
            .join("main.lua"),
        edited,
    )
    .expect("file is edited");
    h.fx.server.execute("restart hello");

    let notice = h.expect_notice(
        &mut client,
        "the restarted resource",
        |notice| matches!(notice, ClientNotice::ResourceStarted { name, .. } if name == "hello"),
    );
    assert_eq!(
        notice,
        ClientNotice::ResourceStarted {
            name: "hello".to_owned(),
            downloaded: 1,
            cached: 1,
        }
    );
    let after = client.resources()[0].files[0].path.clone();
    assert_ne!(after, before, "the changed file has a new hash");
    assert_eq!(read(&after), edited);
}

#[test]
fn stopped_resources_are_reported_and_leave_the_list() {
    let h = Harness::new();
    add_example_resources(&h.fx);
    h.fx.server.execute("ensure hello");
    let mut client = h.connect(&h.fx.root().join("cache"));
    assert_eq!(client.resources().len(), 1);

    h.fx.server.execute("stop hello");
    let notice = h.expect_notice(&mut client, "the stop", |notice| {
        matches!(notice, ClientNotice::ResourceStopped { .. })
    });
    assert_eq!(
        notice,
        ClientNotice::ResourceStopped {
            name: "hello".to_owned()
        }
    );
    assert_eq!(client.resources().len(), 0);
}

#[test]
fn resource_whose_client_files_cannot_be_expanded_starts_without_files() {
    let h = Harness::new();
    h.fx.add_resource("broken", &client_manifest(&[], &[], &["missing.txt"]), &[]);
    h.fx.server.execute("ensure broken");
    assert!(h.fx.manager().is_running("broken"));

    let client = h.connect(&h.fx.root().join("cache"));
    let names: Vec<(&str, usize)> = client
        .resources()
        .iter()
        .map(|resource| (resource.name.as_str(), resource.files.len()))
        .collect();
    assert_eq!(names, [("broken", 0)]);
}
