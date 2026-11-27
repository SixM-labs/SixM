//! Networked entities that a client creates, the control requests it makes, and
//! the server functions that give an entity an owner or an orphan rule.
//!
//! The test host records what reaches the server: a client's creation requests
//! and control requests, which the server answers on its own side.

mod common;

use common::{call, new_client_host, new_host, start};
use sixm_core::{EntityInfo, EntityKind, Host, joaat};

#[test]
fn a_client_entity_is_usable_at_once_and_is_owned_by_the_client() {
    let host = new_client_host(4);
    let rt = start(
        &host,
        "maker",
        &["check"],
        &[(
            "main.lua",
            "exports('check', function()\n\
               local v = CreateVehicle('adder', 1.0, 2.0, 3.0, 90.0, true, false)\n\
               assert(v ~= nil and v ~= 0, 'the call returns the id at once')\n\
               assert(DoesEntityExist(v))\n\
               assert(GetEntityType(v) == 2)\n\
               assert(NetworkGetEntityOwner(v) == 4, 'the client owns what it creates')\n\
               SetEntityCoords(v, 5.0, 6.0, 7.0)\n\
               local c = GetEntityCoords(v)\n\
               assert(c.x == 5.0 and c.y == 6.0 and c.z == 7.0)\n\
             end)\n",
        )],
    );

    call(&rt, "check", &[]);
    let created = host.created_requests.borrow().clone();
    assert_eq!(created.len(), 1, "one creation request reaches the server");
    assert_eq!(
        host.entity(created[0]),
        Some(EntityInfo {
            kind: EntityKind::Vehicle,
            model: joaat(b"adder"),
            position: [5.0, 6.0, 7.0],
            rotation: [0.0, 0.0, 90.0],
            owner: Some(4),
        })
    );
    assert_eq!(
        host.owned_moves.borrow().len(),
        1,
        "the move of the new entity is sent"
    );
}

#[test]
fn a_client_refuses_to_create_an_entity_that_is_not_networked() {
    let host = new_client_host(4);
    let rt = start(
        &host,
        "local_only",
        &["check"],
        &[(
            "main.lua",
            "exports('check', function()\n\
               local ok = pcall(CreateVehicle, 'adder', 1.0, 2.0, 3.0, 0.0, false, false)\n\
               assert(not ok, 'a local entity is refused')\n\
               ok = pcall(CreateObject, 'prop_crate', 1.0, 2.0, 3.0)\n\
               assert(not ok, 'a missing isNetwork is refused')\n\
             end)\n",
        )],
    );

    call(&rt, "check", &[]);
    assert!(host.created_requests.borrow().is_empty());
}

#[test]
fn a_client_asks_for_control_of_an_unowned_entity_only() {
    let host = new_client_host(4);
    let unowned = host.create_entity_for_test(EntityKind::Ped, None);
    let owned = host.create_entity_for_test(EntityKind::Ped, Some(9));
    let rt = start(
        &host,
        "control",
        &["check"],
        &[(
            "main.lua",
            &format!(
                "exports('check', function()\n\
                   assert(NetworkRequestControlOfEntity({unowned}) == true)\n\
                   assert(NetworkRequestControlOfEntity({owned}) == false)\n\
                   assert(NetworkRequestControlOfEntity(999) == false)\n\
                 end)\n"
            ),
        )],
    );

    call(&rt, "check", &[]);
    assert_eq!(*host.control_requests.borrow(), vec![unowned]);
}

#[test]
fn the_server_gives_an_entity_an_owner_and_an_orphan_rule() {
    let host = new_host();
    let rt = start(
        &host,
        "owners",
        &["check"],
        &[(
            "main.lua",
            "exports('check', function()\n\
               local v = CreateVehicle('adder', 0.0, 0.0, 0.0, 0.0)\n\
               assert(SetEntityOwner(v, 7) == true)\n\
               assert(NetworkGetEntityOwner(v) == 7)\n\
               assert(SetEntityOwner(v, 0) == true)\n\
               assert(NetworkGetEntityOwner(v) == -1, 'owner 0 is the server')\n\
               assert(SetEntityOwner(999, 7) == false, 'an unknown entity')\n\
               SetEntityOrphanMode(v, 'migrate')\n\
               assert(not pcall(SetEntityOrphanMode, v, 'keep'), 'an unknown mode')\n\
             end)\n",
        )],
    );

    call(&rt, "check", &[]);
    let vehicle = 1;
    assert_eq!(
        host.orphan_modes.borrow().get(&vehicle).map(String::as_str),
        Some("migrate")
    );
}
