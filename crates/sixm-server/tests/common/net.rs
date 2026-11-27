//! A server on loopback with resources running, driven from the test thread.
//!
//! The test thread plays the server loop: it pumps the server's input and ticks
//! it while it waits, because `NetClient::connect` and `recv_timeout` both block.

use std::net::SocketAddr;
use std::sync::mpsc::{self, Receiver};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use sixm_core::{Host, Value};
use sixm_net::{ClientEvent, Identity, NetClient, NetError};
use sixm_server::Input;

use super::{Fixture, manifest};

/// Longest a test waits for something the server should do.
pub const TIMEOUT: Duration = Duration::from_secs(10);
/// How long each wait step blocks before pumping the server again.
pub const STEP: Duration = Duration::from_millis(5);

/// A resource to run on the loopback server.
pub struct Resource<'a> {
    /// Folder and resource name.
    pub name: &'a str,
    /// Source of `server.lua`.
    pub script: &'a str,
    /// Names the script exports.
    pub exports: &'a [&'a str],
}

/// A server listening on loopback with resources running.
pub struct Loopback {
    pub fx: Fixture,
    /// The first resource given to [`new`](Self::new). `call` and `log` use it.
    primary: String,
    input: Receiver<Input>,
    pub addr: SocketAddr,
}

impl Loopback {
    /// Starts a server with `resources` running. The first resource is the primary one.
    pub fn new(resources: &[Resource<'_>]) -> Self {
        let fx = Fixture::new();
        for resource in resources {
            fx.add_resource(
                resource.name,
                &manifest(&["server.lua"], resource.exports, &[]),
                &[("server.lua", resource.script)],
            );
        }
        fx.server.execute("endpoint_add_udp 127.0.0.1:0");
        let (sender, input) = mpsc::channel();
        let addr = fx
            .server
            .start_network(sender)
            .expect("the network starts on loopback");
        for resource in resources {
            fx.server.execute(&format!("ensure {}", resource.name));
        }
        let primary = resources[0].name.to_owned();
        Self {
            fx,
            primary,
            input,
            addr,
        }
    }

    /// Handles every queued network input and runs one frame.
    pub fn pump(&self) {
        while let Ok(input) = self.input.try_recv() {
            self.fx.server.handle(input);
        }
        self.fx.server.tick(Instant::now());
    }

    /// Pumps the server until `check` returns a value, or fails after [`TIMEOUT`].
    pub fn wait_for<T>(&self, what: &str, mut check: impl FnMut() -> Option<T>) -> T {
        let deadline = Instant::now() + TIMEOUT;
        loop {
            self.pump();
            if let Some(value) = check() {
                return value;
            }
            assert!(Instant::now() < deadline, "timed out waiting for {what}");
            thread::sleep(STEP);
        }
    }

    /// Connects a client named `name`, pumping the server during the handshake.
    pub fn connect(&self, name: &str) -> Result<NetClient, NetError> {
        self.connect_observed(name, |_| {})
    }

    /// Like [`connect`](Self::connect), and passes every progress message of
    /// the server to `on_deferral`.
    pub fn connect_observed(
        &self,
        name: &str,
        on_deferral: impl FnMut(&str) + Send + 'static,
    ) -> Result<NetClient, NetError> {
        let handshake = self.spawn_connect(name, on_deferral, TIMEOUT);
        self.finish_connect(handshake)
    }

    /// Pumps the server until the handshake started by
    /// [`spawn_connect`](Self::spawn_connect) finishes, and returns its result.
    pub fn finish_connect(
        &self,
        handshake: JoinHandle<Result<NetClient, NetError>>,
    ) -> Result<NetClient, NetError> {
        loop {
            self.pump();
            if handshake.is_finished() {
                return handshake.join().expect("the client thread does not panic");
            }
            thread::sleep(STEP);
        }
    }

    /// Starts a client handshake on its own thread, without pumping the
    /// server. The caller pumps until the handle finishes.
    pub fn spawn_connect(
        &self,
        name: &str,
        on_deferral: impl FnMut(&str) + Send + 'static,
        timeout: Duration,
    ) -> JoinHandle<Result<NetClient, NetError>> {
        let addr = self.addr;
        let name = name.to_owned();
        thread::spawn(move || {
            NetClient::connect_observed(addr, &name, &Identity::Ephemeral, timeout, on_deferral)
        })
    }

    /// Waits for the next event named `name` on `client` and returns its arguments.
    pub fn expect_event(&self, client: &NetClient, name: &str) -> Vec<Value> {
        self.wait_for(&format!("event {name}"), || {
            match client.recv_timeout(STEP) {
                Some(ClientEvent::Event {
                    name: received,
                    args,
                }) if received == name => Some(args),
                _ => None,
            }
        })
    }

    /// Calls `export` of the primary resource with `args`, discarding its results.
    pub fn call(&self, export: &str, args: &[Value]) {
        self.call_in(&self.primary, export, args);
    }

    /// Calls `export` of `resource` with `args`, discarding its results.
    pub fn call_in(&self, resource: &str, export: &str, args: &[Value]) {
        self.fx
            .manager()
            .call_export(resource, export, args, None)
            .unwrap_or_else(|err| panic!("export `{export}` of `{resource}`: {err}"));
    }

    /// The `log` export of the primary resource as one string.
    pub fn log(&self) -> String {
        self.fx.text(&self.primary, "log")
    }

    /// Waits until the primary resource's log contains `entry`.
    pub fn wait_for_log(&self, entry: &str) {
        self.wait_for(&format!("log entry `{entry}`"), || {
            self.log().contains(entry).then_some(())
        });
    }
}
