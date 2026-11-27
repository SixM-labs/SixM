//! Client-created entities end to end: a real `sixm_client::Client` on loopback
//! asks the server for entities under its leased ids, the server confirms or
//! refuses them, ownership moves to the client that asks for an unowned entity,
//! and the entities a client created go with it when it leaves.
//!
//! The test thread plays the server loop, as in `client_sync.rs`.

mod common;

use std::net::SocketAddr;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::thread;
use std::time::{Duration, Instant};

use common::Fixture;
use sixm_client::{Client, ClientNotice};
use sixm_core::{EntityInfo, EntityKind, Host, ServerHost, joaat};
use sixm_net::Identity;
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

    /// Connects a client with its own cache folder. The server is pumped until
    /// the client has connected and synced.
    fn connect(&self, cache: &std::path::Path) -> Client {
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

    /// Pumps the server until `done` holds.
    fn until(&self, what: &str, done: impl Fn(&Fixture) -> bool) {
        let deadline = Instant::now() + TIMEOUT;
        while !done(&self.fx) {
            self.pump();
            assert!(Instant::now() < deadline, "timed out waiting for {what}");
            thread::sleep(STEP);
        }
    }
}

/// The lease the server sent once the client joined: its first id.
fn first_lease(harness: &Harness, client: &mut Client) -> u32 {
    match harness.expect_notice(client, "an id lease", |notice| {
        matches!(notice, ClientNotice::EntityIdLease { .. })
    }) {
        ClientNotice::EntityIdLease { first, .. } => first,
        other => panic!("unexpected notice {other:?}"),
    }
}

#[test]
fn a_client_entity_is_confirmed_and_goes_with_its_creator() {
    let harness = Harness::new();
    let cache = tempfile::tempdir().expect("temp dir");
    let mut client = harness.connect(cache.path());
    let first = first_lease(&harness, &mut client);

    client
        .send_create_entity(first, 2, joaat(b"adder"), [1.0, 2.0, 3.0], [0.0, 0.0, 90.0])
        .expect("the request is sent");
    let confirmed = harness.expect_notice(
        &mut client,
        "the confirmation",
        |notice| matches!(notice, ClientNotice::EntityCreated { id, .. } if *id == first),
    );
    let ClientNotice::EntityCreated { info, .. } = confirmed else {
        panic!("unexpected notice");
    };
    assert_eq!(info.owner, Some(client.player()), "the creator owns it");
    assert_eq!(info.kind, EntityKind::Vehicle);
    assert_eq!(
        harness.fx.manager().entity(first).map(|info| info.owner),
        Some(Some(client.player()))
    );

    drop(client);
    harness.until("the entity of the departed creator to be deleted", |fx| {
        fx.manager().entity(first).is_none()
    });
}

#[test]
fn a_client_over_its_limit_is_refused_and_its_id_is_answered() {
    let harness = Harness::new();
    harness.fx.server.execute("setr sixm_maxClientEntities 1");
    let cache = tempfile::tempdir().expect("temp dir");
    let mut client = harness.connect(cache.path());
    let first = first_lease(&harness, &mut client);

    client
        .send_create_entity(first, 3, joaat(b"prop_crate"), [0.0; 3], [0.0; 3])
        .expect("the first request is sent");
    harness.expect_notice(
        &mut client,
        "the first confirmation",
        |notice| matches!(notice, ClientNotice::EntityCreated { id, .. } if *id == first),
    );

    client
        .send_create_entity(first + 1, 3, joaat(b"prop_crate"), [0.0; 3], [0.0; 3])
        .expect("the second request is sent");
    let refused = harness.expect_notice(&mut client, "the refusal", |notice| {
        matches!(notice, ClientNotice::EntityCreateRefused { .. })
    });
    let ClientNotice::EntityCreateRefused { id, reason } = refused else {
        panic!("unexpected notice");
    };
    assert_eq!(id, first + 1);
    assert!(reason.contains("maximum of 1"), "{reason}");
    assert!(harness.fx.manager().entity(first + 1).is_none());
}

fn near_pos(actual: [f32; 3], expected: [f32; 3]) -> bool {
    (actual[0] - expected[0]).abs() < 0.02
        && (actual[1] - expected[1]).abs() < 0.02
        && (actual[2] - expected[2]).abs() < 0.02
}

#[test]
fn a_client_gets_an_unowned_entity_and_its_first_move_is_accepted() {
    let harness = Harness::new();
    let cache = tempfile::tempdir().expect("temp dir");
    let mut client = harness.connect(cache.path());
    // A script entity at the origin, where the client's own ped stands, so the
    // client holds it.
    let shared = harness
        .fx
        .manager()
        .create_entity(EntityInfo {
            kind: EntityKind::Object,
            model: joaat(b"prop_crate"),
            position: [0.0; 3],
            rotation: [0.0; 3],
            owner: None,
        })
        .expect("the server creates the entity");
    harness.expect_notice(
        &mut client,
        "the shared entity",
        |notice| matches!(notice, ClientNotice::EntityCreated { id, .. } if *id == shared),
    );

    client
        .send_request_entity_control(shared)
        .expect("the request is sent");
    let granted = harness.expect_notice(
        &mut client,
        "the ownership",
        |notice| matches!(notice, ClientNotice::EntityOwner { id, .. } if *id == shared),
    );
    let ClientNotice::EntityOwner { owner, epoch, .. } = granted else {
        panic!("unexpected notice");
    };
    assert_eq!(owner, client.player());
    assert_eq!(epoch, 0, "a fresh entity starts at the current epoch");

    // The new owner moves the entity with the epoch it was given, and the
    // server accepts it.
    client
        .send_entity_transforms(&[(shared, Some([4.0, 5.0, 6.0]), None, epoch)])
        .expect("the transform is sent");
    harness.until("the owner's move to be applied", |fx| {
        fx.manager()
            .entity(shared)
            .is_some_and(|info| near_pos(info.position, [4.0, 5.0, 6.0]))
    });
}

#[test]
fn lockdown_convar_and_bucket_override_gate_client_creation() {
    let harness = Harness::new();
    harness.fx.server.execute("set sv_entityLockdown strict");
    let cache = tempfile::tempdir().expect("temp dir");
    let mut client = harness.connect(cache.path());
    let first = first_lease(&harness, &mut client);

    // Bucket 0 inherits `sv_entityLockdown strict`, so creation is refused.
    client
        .send_create_entity(first, 2, joaat(b"adder"), [0.0; 3], [0.0; 3])
        .expect("the request is sent");
    let refused = harness.expect_notice(
        &mut client,
        "strict lockdown refusal",
        |notice| matches!(notice, ClientNotice::EntityCreateRefused { id, .. } if *id == first),
    );
    assert!(matches!(
        refused,
        ClientNotice::EntityCreateRefused { reason, .. } if reason.contains("locked down")
    ));

    // Overriding bucket 1 to `relaxed` allows vehicles while still blocking peds.
    harness
        .fx
        .manager()
        .set_routing_bucket_entity_lockdown_mode(1, "relaxed")
        .expect("valid lockdown mode");
    harness
        .fx
        .manager()
        .set_player_routing_bucket(client.player(), 1);

    client
        .send_create_entity(first + 1, 1, joaat(b"a_m_m_skater_01"), [0.0; 3], [0.0; 3])
        .expect("the ped request is sent");
    let refused = harness.expect_notice(
        &mut client,
        "relaxed ped refusal",
        |notice| matches!(notice, ClientNotice::EntityCreateRefused { id, .. } if *id == first + 1),
    );
    assert!(matches!(
        refused,
        ClientNotice::EntityCreateRefused { reason, .. } if reason.contains("peds")
    ));

    client
        .send_create_entity(first + 2, 2, joaat(b"adder"), [1.0, 2.0, 3.0], [0.0; 3])
        .expect("the vehicle request is sent");
    harness.expect_notice(
        &mut client,
        "vehicle confirmation",
        |notice| matches!(notice, ClientNotice::EntityCreated { id, .. } if *id == first + 2),
    );
}

#[test]
fn migrating_entity_moves_to_nearest_in_scope_player_keeping_epoch() {
    let harness = Harness::new();
    let cache_a = tempfile::tempdir().expect("temp dir a");
    let cache_b = tempfile::tempdir().expect("temp dir b");
    let cache_c = tempfile::tempdir().expect("temp dir c");
    let mut a = harness.connect(cache_a.path());
    let mut b = harness.connect(cache_b.path());
    let mut c = harness.connect(cache_c.path());

    // Place B near (10, 0, 0) and C farther at (100, 0, 0).
    let ped_b = harness.fx.manager().player_ped(b.player()).expect("ped b");
    let ped_c = harness.fx.manager().player_ped(c.player()).expect("ped c");
    harness
        .fx
        .manager()
        .set_entity_position(ped_b, [10.0, 0.0, 0.0])
        .expect("move b");
    harness
        .fx
        .manager()
        .set_entity_position(ped_c, [100.0, 0.0, 0.0])
        .expect("move c");

    let first = first_lease(&harness, &mut a);
    a.send_create_entity(first, 2, joaat(b"adder"), [8.0, 0.0, 0.0], [0.0; 3])
        .expect("create request");
    harness.expect_notice(
        &mut a,
        "created on a",
        |notice| matches!(notice, ClientNotice::EntityCreated { id, .. } if *id == first),
    );
    harness
        .fx
        .manager()
        .set_entity_orphan_mode(first, "migrate")
        .expect("migrate mode");
    // Advance the teleport epoch via a server move before migration.
    harness
        .fx
        .manager()
        .set_entity_position(first, [9.0, 0.0, 0.0])
        .expect("server move");
    harness.fx.server.tick(Instant::now());

    // When A leaves, B (nearest in scope) becomes owner and inherits epoch 1.
    drop(a);
    let b_id = b.player();
    let notice = harness.expect_notice(&mut b, "ownership migrated to b", |notice| {
        matches!(notice, ClientNotice::EntityOwner { id, owner, .. } if *id == first && *owner == b_id)
    });
    let ClientNotice::EntityOwner { epoch, .. } = notice else {
        panic!("unexpected notice");
    };
    assert_eq!(epoch, 1, "teleport epoch is preserved on migration");

    // B moves it with the inherited epoch, and the server accepts it.
    b.send_entity_transforms(&[(first, Some([12.0, 0.0, 0.0]), None, epoch)])
        .expect("move from new owner");
    harness.until("new owner move applied", |fx| {
        fx.manager()
            .entity(first)
            .is_some_and(|info| near_pos(info.position, [12.0, 0.0, 0.0]))
    });

    // When B moves to another routing bucket, C (the remaining in-scope holder in bucket 0) takes over.
    let c_id = c.player();
    harness.fx.manager().set_player_routing_bucket(b_id, 2);
    harness.expect_notice(&mut c, "ownership migrated to c", |notice| {
        matches!(notice, ClientNotice::EntityOwner { id, owner, .. } if *id == first && *owner == c_id)
    });

    // When C also leaves, ownership returns to the server (owner = None).
    drop(c);
    harness.until("entity reverts to server ownership", |fx| {
        fx.manager()
            .entity(first)
            .is_some_and(|info| info.owner.is_none())
    });
}
