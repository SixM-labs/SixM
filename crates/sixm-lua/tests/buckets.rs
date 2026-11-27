//! Routing bucket functions on the server. The scripts check their own
//! results with asserts, against a host that records the buckets it is given.

mod common;

use common::{call, new_host, start};
use sixm_core::PlayerInfo;

/// Connects player `id` to the host.
fn connect(host: &common::TestHost, id: u32) {
    host.players.replace(vec![(
        id,
        PlayerInfo {
            name: format!("player {id}"),
            endpoint: format!("203.0.113.{id}:51234"),
        },
    )]);
}

#[test]
fn players_and_entities_start_in_bucket_zero_and_follow_the_setters() {
    let host = new_host();
    connect(&host, 1);
    let rt = start(
        &host,
        "buckets",
        &["check"],
        &[(
            "main.lua",
            "exports('check', function()\n\
               assert(GetPlayerRoutingBucket(1) == 0)\n\
               SetPlayerRoutingBucket(1, 5)\n\
               assert(GetPlayerRoutingBucket(1) == 5)\n\
               assert(GetPlayersInRoutingBucket(5)[1] == 1)\n\
               assert(#GetPlayersInRoutingBucket(0) == 0)\n\
               local v = CreateVehicle('adder', 0.0, 0.0, 0.0, 0.0, true, false)\n\
               assert(GetEntityRoutingBucket(v) == 0)\n\
               SetEntityRoutingBucket(v, 3)\n\
               assert(GetEntityRoutingBucket(v) == 3)\n\
               SetEntityRoutingBucket(99, 3)\n\
               assert(GetEntityRoutingBucket(99) == 0)\n\
               SetPlayerRoutingBucket(42, 2)\n\
               assert(GetPlayerRoutingBucket(42) == 0)\n\
               assert(not pcall(SetPlayerRoutingBucket, 1, -1))\n\
               assert(not pcall(SetEntityRoutingBucket, v, 4294967296))\n\
               assert(GetPlayerRoutingBucket(1) == 5)\n\
             end)\n",
        )],
    );

    call(&rt, "check", &[]);
    assert_eq!(host.player_buckets.borrow().get(&1), Some(&5));
    assert!(host.player_buckets.borrow().get(&42).is_none());
}

#[test]
fn population_and_lockdown_are_stored_per_bucket_with_defaults() {
    let host = new_host();
    let rt = start(
        &host,
        "settings",
        &["check"],
        &[(
            "main.lua",
            "exports('check', function()\n\
               assert(GetRoutingBucketPopulationEnabled(9))\n\
               SetRoutingBucketPopulationEnabled(9, false)\n\
               assert(not GetRoutingBucketPopulationEnabled(9))\n\
               assert(GetRoutingBucketEntityLockdownMode(9) == 'inactive')\n\
               SetRoutingBucketEntityLockdownMode(9, 'strict')\n\
               assert(GetRoutingBucketEntityLockdownMode(9) == 'strict')\n\
               assert(not pcall(SetRoutingBucketEntityLockdownMode, 9, 'loose'))\n\
               assert(GetRoutingBucketEntityLockdownMode(9) == 'strict')\n\
             end)\n",
        )],
    );

    call(&rt, "check", &[]);
    assert_eq!(
        host.lockdown.borrow().get(&9).map(String::as_str),
        Some("strict")
    );
}
