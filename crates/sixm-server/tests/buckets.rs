//! Bucket events: `onPlayerBucketChange` and `onEntityBucketChange` fire only
//! when a bucket really changes, after the move, with the ped that moves along.

mod common;

use common::net::{Loopback, Resource};
use sixm_core::{Host, ServerHost, Value};

/// The `buckets` resource: records every bucket event in `LOG` and exposes it.
const BUCKETS_SCRIPT: &str = r"
LOG = {}

local function note(entry)
    LOG[#LOG + 1] = entry
end

AddEventHandler('onPlayerBucketChange', function(player, bucket, oldBucket)
    note('p:' .. player .. ':' .. bucket .. ':' .. oldBucket)
end)

AddEventHandler('onEntityBucketChange', function(entity, bucket, oldBucket)
    note('e:' .. entity .. ':' .. bucket .. ':' .. oldBucket)
end)

function log()
    return table.concat(LOG, ' ')
end

function set_player(player, bucket)
    SetPlayerRoutingBucket(player, bucket)
end

function set_entity(entity, bucket)
    SetEntityRoutingBucket(entity, bucket)
end

function make(kind)
    if kind == 'ped' then
        return CreatePed(4, 'a_m_m_skater_01', 1.0, 0.0, 0.0, 0.0, true, false)
    elseif kind == 'vehicle' then
        return CreateVehicle('adder', 0.0, 0.0, 0.0, 0.0, true, false)
    end
    return CreateObject('prop_crate', 1.0, 0.0, 0.0, true, false, 0)
end
";

/// A server on loopback with the `buckets` resource running.
fn harness() -> Loopback {
    Loopback::new(&[Resource {
        name: "buckets",
        script: BUCKETS_SCRIPT,
        exports: &["log", "set_player", "set_entity", "make"],
    }])
}

/// Creates an entity of `kind` (`ped`, `vehicle` or `object`) and returns its id.
fn make(h: &Loopback, kind: &str) -> i64 {
    let values =
        h.fx.manager()
            .call_export("buckets", "make", &[Value::from(kind)], None)
            .expect("make");
    match values.first() {
        Some(Value::Int(id)) => *id,
        other => panic!("make returned {other:?}"),
    }
}

#[test]
fn bucket_events_fire_only_on_a_real_change() {
    let h = harness();
    let _alice = h.connect("Alice").expect("Alice is admitted");
    let ped = h.fx.manager().player_ped(1).expect("Alice has a ped");
    let ped = i64::from(ped);

    h.call("set_player", &[Value::Int(1), Value::Int(3)]);
    h.call("set_player", &[Value::Int(1), Value::Int(3)]);
    h.call("set_player", &[Value::Int(1), Value::Int(0)]);
    h.call("set_player", &[Value::Int(99), Value::Int(1)]);

    assert_eq!(h.log(), format!("p:1:3:0 e:{ped}:3:0 p:1:0:3 e:{ped}:0:3"));
}

#[test]
fn entity_bucket_events_fire_for_every_entity_kind() {
    let h = harness();
    let mut expected = Vec::new();
    for kind in ["ped", "vehicle", "object"] {
        let id = make(&h, kind);
        h.call("set_entity", &[Value::Int(id), Value::Int(2)]);
        expected.push(format!("e:{id}:2:0"));
        h.call("set_entity", &[Value::Int(id), Value::Int(2)]);
    }
    h.call("set_entity", &[Value::Int(9999), Value::Int(2)]);

    assert_eq!(h.log(), expected.join(" "));
}

#[test]
fn a_ped_that_is_already_in_the_target_bucket_gets_no_second_event() {
    let h = harness();
    let _alice = h.connect("Alice").expect("Alice is admitted");
    let ped = i64::from(h.fx.manager().player_ped(1).expect("Alice has a ped"));

    h.call("set_entity", &[Value::Int(ped), Value::Int(4)]);
    h.call("set_player", &[Value::Int(1), Value::Int(4)]);

    assert_eq!(h.log(), format!("e:{ped}:4:0 p:1:4:0"));
}
