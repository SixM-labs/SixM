//! Entity functions: creation, movement and deletion on the server, the read
//! functions on both sides, and `GetGamePool` on the client.
//!
//! Most scripts run one export, `check`, whose Lua asserts state what must hold.
//! A failed assert makes the export fail, and the test fails with its message.

mod common;

use common::{call, new_client_host, new_host, start};
use sixm_core::{EntityInfo, EntityKind, Host, Value, joaat};

/// A ped that `owner` moves, or a script-created one when `owner` is `None`.
fn ped(owner: Option<u32>) -> EntityInfo {
    EntityInfo {
        kind: EntityKind::Ped,
        model: joaat(b"a_m_m_skater_01"),
        position: [0.0; 3],
        rotation: [0.0; 3],
        owner,
    }
}

#[test]
fn vehicles_are_created_with_their_model_position_and_heading() {
    let host = new_host();
    let rt = start(
        &host,
        "creator",
        &["check"],
        &[(
            "main.lua",
            "exports('check', function()\n\
               local v = CreateVehicle('adder', 1.0, 2.0, 3.0, 90.0, true, false)\n\
               assert(v == 1, 'the first id is 1')\n\
               assert(DoesEntityExist(v))\n\
               assert(GetEntityType(v) == 2)\n\
               assert(GetEntityModel(v) == GetHashKey('adder'))\n\
               local c = GetEntityCoords(v)\n\
               assert(c.x == 1.0 and c.y == 2.0 and c.z == 3.0)\n\
               assert(GetEntityHeading(v) == 90.0)\n\
               assert(NetworkGetNetworkIdFromEntity(v) == v)\n\
               assert(NetworkGetEntityFromNetworkId(v) == v)\n\
               assert(NetworkDoesNetworkIdExist(v))\n\
             end)\n",
        )],
    );

    call(&rt, "check", &[]);
    assert_eq!(
        host.entity(1),
        Some(EntityInfo {
            kind: EntityKind::Vehicle,
            model: joaat(b"adder"),
            position: [1.0, 2.0, 3.0],
            rotation: [0.0, 0.0, 90.0],
            owner: None,
        })
    );
}

#[test]
fn every_creation_native_makes_an_entity_of_its_kind() {
    let host = new_host();
    let rt = start(
        &host,
        "creators",
        &["check"],
        &[(
            "main.lua",
            "exports('check', function()\n\
               local p = CreatePed(4, 'a_m_m_skater_01', 1.0, 1.0, 1.0, 45.0, true, false)\n\
               local v = CreateVehicleServerSetter('adder', 'automobile', 2.0, 2.0, 2.0, 10.0)\n\
               local o = CreateObject('prop_bench_01a', 3.0, 3.0, 3.0, true, false, 0)\n\
               local n = CreateObjectNoOffset('prop_bench_01a', 4.0, 4.0, 4.0, true, false, 0)\n\
               assert(p == 1 and v == 2 and o == 3 and n == 4)\n\
               assert(GetEntityType(p) == 1 and GetEntityType(v) == 2)\n\
               assert(GetEntityType(o) == 3 and GetEntityType(n) == 3)\n\
               assert(GetEntityHeading(p) == 45.0)\n\
               assert(GetEntityHeading(v) == 10.0)\n\
               assert(GetEntityHeading(o) == 0.0 and GetEntityHeading(n) == 0.0)\n\
               assert(#GetAllPeds() == 1 and GetAllPeds()[1] == p)\n\
               assert(#GetAllVehicles() == 1 and GetAllVehicles()[1] == v)\n\
               local objects = GetAllObjects()\n\
               assert(#objects == 2 and objects[1] == o and objects[2] == n)\n\
             end)\n",
        )],
    );

    call(&rt, "check", &[]);
    let objects: Vec<u32> = host
        .entities
        .borrow()
        .iter()
        .filter(|(_, info)| info.kind == EntityKind::Object)
        .map(|(id, _)| *id)
        .collect();
    assert_eq!(objects, vec![3, 4]);
}

#[test]
fn model_strings_and_hashes_name_the_same_model() {
    let host = new_host();
    let rt = start(
        &host,
        "models",
        &["check"],
        &[(
            "main.lua",
            "exports('check', function()\n\
               local a = CreateObject('prop_bench_01a', 0.0, 0.0, 0.0, true, false, 0)\n\
               local b = CreateObject(GetHashKey('prop_bench_01a'), 0.0, 0.0, 0.0, true, false, 0)\n\
               local c = CreateObject(123456, 0.0, 0.0, 0.0, true, false, 0)\n\
               assert(GetEntityModel(a) == GetEntityModel(b))\n\
               assert(GetEntityModel(a) == GetHashKey('prop_bench_01a'))\n\
               assert(GetEntityModel(c) == 123456)\n\
               assert(not pcall(CreateObject, {}, 0.0, 0.0, 0.0, true, false, 0))\n\
             end)\n",
        )],
    );

    call(&rt, "check", &[]);
    assert_eq!(
        host.entity(1).map(|info| info.model),
        Some(joaat(b"prop_bench_01a"))
    );
}

#[test]
fn negative_hash_keys_round_trip_as_models() {
    let host = new_host();
    let rt = start(
        &host,
        "hashes",
        &["check"],
        &[(
            "main.lua",
            "exports('check', function()\n\
               local hash = GetHashKey('adder')\n\
               assert(hash < 0, 'GetHashKey returns a signed value')\n\
               local v = CreateVehicle(hash, 0.0, 0.0, 0.0, 0.0, true, false)\n\
               assert(GetEntityModel(v) == hash)\n\
             end)\n",
        )],
    );

    call(&rt, "check", &[]);
    assert_eq!(host.entity(1).map(|info| info.model), Some(joaat(b"adder")));
}

#[test]
fn unknown_entities_read_as_neutral_values_and_setters_do_nothing() {
    let host = new_host();
    let rt = start(
        &host,
        "unknown",
        &["check"],
        &[(
            "main.lua",
            "exports('check', function()\n\
               assert(not DoesEntityExist(99))\n\
               local c = GetEntityCoords(99)\n\
               assert(c.x == 0.0 and c.y == 0.0 and c.z == 0.0)\n\
               assert(GetEntityHeading(99) == 0.0)\n\
               local r = GetEntityRotation(99)\n\
               assert(r.x == 0.0 and r.y == 0.0 and r.z == 0.0)\n\
               assert(GetEntityModel(99) == 0)\n\
               assert(GetEntityType(99) == 0)\n\
               assert(NetworkGetNetworkIdFromEntity(99) == 0)\n\
               assert(NetworkGetEntityFromNetworkId(99) == 0)\n\
               assert(not NetworkDoesNetworkIdExist(99))\n\
               assert(GetEntityFromStateBagName('entity:99') == 0)\n\
               SetEntityCoords(99, 1.0, 2.0, 3.0)\n\
               SetEntityCoordsNoOffset(99, 1.0, 2.0, 3.0)\n\
               SetEntityHeading(99, 10.0)\n\
               SetEntityRotation(99, 1.0, 2.0, 3.0)\n\
               DeleteEntity(99)\n\
               assert(not DoesEntityExist(99))\n\
               assert(#GetAllVehicles() == 0)\n\
               assert(GetGamePool == nil, 'the game pool is client-only')\n\
             end)\n",
        )],
    );

    call(&rt, "check", &[]);
    assert!(host.entities.borrow().is_empty());
}

#[test]
fn non_finite_numbers_raise_errors_and_change_nothing() {
    let host = new_host();
    let rt = start(
        &host,
        "finite",
        &["check"],
        &[(
            "main.lua",
            "exports('check', function()\n\
               local v = CreateVehicle('adder', 0.0, 0.0, 0.0, 0.0, true, false)\n\
               local nan = 0.0 / 0.0\n\
               local inf = 1.0 / 0.0\n\
               assert(not pcall(SetEntityCoords, v, nan, 0.0, 0.0))\n\
               assert(not pcall(SetEntityCoords, v, 0.0, inf, 0.0))\n\
               assert(not pcall(SetEntityCoordsNoOffset, v, 0.0, 0.0, -inf))\n\
               assert(not pcall(SetEntityHeading, v, inf))\n\
               assert(not pcall(SetEntityRotation, v, 0.0, nan, 0.0))\n\
               assert(not pcall(CreateVehicle, 'adder', inf, 0.0, 0.0, 0.0, true, false))\n\
               assert(not pcall(CreateObject, 'prop', 0.0, nan, 0.0, true, false, 0))\n\
               assert(not pcall(SetEntityCoords, v, 'far', 0.0, 0.0))\n\
               local c = GetEntityCoords(v)\n\
               assert(c.x == 0.0 and c.y == 0.0 and c.z == 0.0)\n\
             end)\n",
        )],
    );

    call(&rt, "check", &[]);
    assert_eq!(host.entity(1).map(|info| info.position), Some([0.0; 3]));
}

#[test]
fn moves_keep_the_latest_transform_and_heading_keeps_pitch_and_roll() {
    let host = new_host();
    let rt = start(
        &host,
        "moves",
        &["check"],
        &[(
            "main.lua",
            "exports('check', function()\n\
               local v = CreateVehicle('adder', 0.0, 0.0, 0.0, 0.0, true, false)\n\
               SetEntityCoords(v, 1.0, 1.0, 1.0)\n\
               SetEntityCoords(v, 5.0, 6.0, 7.0)\n\
               SetEntityCoordsNoOffset(v, 8.0, 9.0, 10.0)\n\
               SetEntityRotation(v, 10.0, 20.0, 30.0)\n\
               SetEntityHeading(v, 90.0)\n\
               local c = GetEntityCoords(v)\n\
               assert(c.x == 8.0 and c.y == 9.0 and c.z == 10.0)\n\
               local r = GetEntityRotation(v, 2)\n\
               assert(r.x == 10.0 and r.y == 20.0 and r.z == 90.0)\n\
               assert(GetEntityHeading(v) == 90.0)\n\
             end)\n",
        )],
    );

    call(&rt, "check", &[]);
    let info = host.entity(1).expect("the vehicle exists");
    assert_eq!(info.position, [8.0, 9.0, 10.0]);
    assert_eq!(info.rotation, [10.0, 20.0, 90.0]);
}

#[test]
fn deleting_an_entity_removes_it_and_its_state_bag() {
    let host = new_host();
    let rt = start(
        &host,
        "deletes",
        &["check"],
        &[(
            "main.lua",
            "exports('check', function()\n\
               local v = CreateVehicle('adder', 0.0, 0.0, 0.0, 0.0, true, false)\n\
               Entity(v).state.plate = 'SIXM'\n\
               assert(Entity(v).state.plate == 'SIXM')\n\
               DeleteEntity(v)\n\
               assert(not DoesEntityExist(v))\n\
               assert(Entity(v).state.plate == nil)\n\
               assert(not pcall(function() Entity(v).state.plate = 'again' end))\n\
             end)\n",
        )],
    );

    call(&rt, "check", &[]);
    assert!(host.entities.borrow().is_empty());
    assert_eq!(host.state_get("entity:1", "plate"), None);
}

#[test]
fn entity_state_bags_hold_values_and_only_exist_for_entities() {
    let host = new_host();
    let rt = start(
        &host,
        "state",
        &["check"],
        &[(
            "main.lua",
            "exports('check', function()\n\
               local v = CreateVehicle('adder', 0.0, 0.0, 0.0, 0.0, true, false)\n\
               Entity(v).state.plate = 'SIXM'\n\
               assert(Entity(v).state.plate == 'SIXM')\n\
               assert(GetEntityFromStateBagName('entity:' .. v) == v)\n\
               assert(GetEntityFromStateBagName('player:1') == 0)\n\
               assert(not pcall(function() Entity(99).state.x = 1 end))\n\
               assert(not pcall(Entity, 'nope'))\n\
               assert(not pcall(Entity, nil))\n\
             end)\n",
        )],
    );

    call(&rt, "check", &[]);
    assert_eq!(
        host.state_get("entity:1", "plate"),
        Some(Value::from("SIXM"))
    );
}

#[test]
fn the_pool_of_a_client_lists_the_entities_it_mirrors() {
    let host = new_client_host(1);
    host.entities.borrow_mut().insert(
        7,
        EntityInfo {
            kind: EntityKind::Vehicle,
            model: joaat(b"adder"),
            position: [1.0, 2.0, 3.0],
            rotation: [0.0, 0.0, 90.0],
            owner: None,
        },
    );
    host.entities.borrow_mut().insert(
        8,
        EntityInfo {
            kind: EntityKind::Ped,
            model: joaat(b"a_m_m_skater_01"),
            position: [0.0; 3],
            rotation: [0.0; 3],
            owner: None,
        },
    );
    let rt = start(
        &host,
        "mirror",
        &["check"],
        &[(
            "main.lua",
            "exports('check', function()\n\
               assert(DoesEntityExist(7) and not DoesEntityExist(9))\n\
               local c = GetEntityCoords(7)\n\
               assert(c.x == 1.0 and c.y == 2.0 and c.z == 3.0)\n\
               assert(GetEntityHeading(7) == 90.0)\n\
               assert(GetEntityRotation(7).z == 90.0)\n\
               assert(GetEntityModel(7) == GetHashKey('adder'))\n\
               assert(GetEntityType(7) == 2 and GetEntityType(8) == 1)\n\
               assert(NetworkGetNetworkIdFromEntity(7) == 7)\n\
               assert(NetworkGetEntityFromNetworkId(7) == 7)\n\
               assert(NetworkGetEntityFromNetworkId(9) == 0)\n\
               assert(NetworkDoesNetworkIdExist(8) and not NetworkDoesNetworkIdExist(9))\n\
               assert(GetEntityFromStateBagName('entity:7') == 7)\n\
               assert(GetEntityFromStateBagName('entity:9') == 0)\n\
               local vehicles = GetGamePool('CVehicle')\n\
               assert(#vehicles == 1 and vehicles[1] == 7)\n\
               local peds = GetGamePool('CPed')\n\
               assert(#peds == 1 and peds[1] == 8)\n\
               assert(#GetGamePool('CObject') == 0)\n\
               assert(#GetGamePool('CNothing') == 0)\n\
               assert(CreateVehicleServerSetter == nil and DeleteEntity == nil)\n\
             end)\n",
        )],
    );

    call(&rt, "check", &[]);
}

#[test]
fn server_reads_player_peds_and_entity_owners() {
    let host = new_host();
    host.player_peds.borrow_mut().insert(1, 7);
    host.entities.borrow_mut().insert(7, ped(Some(1)));
    let rt = start(
        &host,
        "owners",
        &["check"],
        &[(
            "main.lua",
            "exports('check', function()\n\
               assert(GetPlayerPed(1) == 7)\n\
               assert(GetPlayerPed(2) == 0, 'a player without a ped')\n\
               assert(GetPlayerPed(-1) == 0, 'no player is named -1')\n\
               assert(NetworkGetEntityOwner(7) == 1)\n\
               assert(NetworkGetEntityOwner(99) == -1, 'an unknown entity')\n\
               local v = CreateVehicle('adder', 0.0, 0.0, 0.0, 0.0, true, false)\n\
               assert(NetworkGetEntityOwner(v) == -1, 'a script-created entity')\n\
               SetEntityCoords(GetPlayerPed(1), 4.0, 5.0, 6.0)\n\
             end)\n",
        )],
    );

    call(&rt, "check", &[]);
    assert_eq!(
        host.entity(7).map(|info| info.position),
        Some([4.0, 5.0, 6.0])
    );
}

#[test]
fn client_moves_only_the_entities_it_owns_and_keeps_pitch_and_roll() {
    let host = new_client_host(3);
    host.local_ped.set(Some(7));
    host.entities.borrow_mut().insert(7, ped(Some(3)));
    host.entities.borrow_mut().insert(8, ped(Some(4)));
    let rt = start(
        &host,
        "mover",
        &["check"],
        &[(
            "main.lua",
            "exports('check', function()\n\
               assert(PlayerPedId() == 7)\n\
               assert(GetPlayerPed(-1) == 7 and GetPlayerPed(PlayerId()) == 7)\n\
               assert(GetPlayerPed(4) == 0, 'only the local player is known')\n\
               assert(NetworkGetEntityOwner(7) == 3 and NetworkGetEntityOwner(8) == 4)\n\
               assert(SetEntityOwner == nil, 'ownership assignment is server only')\n\
               SetEntityCoords(PlayerPedId(), 5.0, 6.0, 7.0)\n\
               SetEntityCoords(8, 1.0, 1.0, 1.0)\n\
               SetEntityHeading(8, 90.0)\n\
               SetEntityRotation(8, 1.0, 2.0, 3.0)\n\
               assert(not pcall(SetEntityCoords, 8, 0.0 / 0.0, 0.0, 0.0), 'not owned, still checked')\n\
               assert(not pcall(SetEntityCoords, 7, 0.0, 1.0 / 0.0, 0.0))\n\
               SetEntityHeading(7, 90.0)\n\
               SetEntityRotation(7, 10.0, 20.0, 30.0)\n\
               SetEntityHeading(7, 45.0)\n\
               local c = GetEntityCoords(7)\n\
               assert(c.x == 5.0 and c.y == 6.0 and c.z == 7.0)\n\
               assert(GetEntityCoords(8).x == 0.0, 'a move of a foreign entity is not seen')\n\
             end)\n",
        )],
    );

    call(&rt, "check", &[]);
    assert_eq!(
        host.entity(7).map(|info| info.position),
        Some([5.0, 6.0, 7.0])
    );
    assert_eq!(
        host.entity(7).map(|info| info.rotation),
        Some([10.0, 20.0, 45.0])
    );
    assert_eq!(host.entity(8).map(|info| info.position), Some([0.0; 3]));
    assert_eq!(
        *host.owned_moves.borrow(),
        vec![
            (7, Some([5.0, 6.0, 7.0]), None),
            (7, None, Some([0.0, 0.0, 90.0])),
            (7, None, Some([10.0, 20.0, 30.0])),
            (7, None, Some([10.0, 20.0, 45.0])),
        ],
        "only owned moves reach the host, one per call"
    );
}

#[test]
fn a_client_without_a_local_ped_reports_zero() {
    let host = new_client_host(3);
    let rt = start(
        &host,
        "no_ped",
        &["check"],
        &[(
            "main.lua",
            "exports('check', function()\n\
               assert(PlayerPedId() == 0)\n\
               assert(GetPlayerPed(-1) == 0)\n\
             end)\n",
        )],
    );

    call(&rt, "check", &[]);
}
