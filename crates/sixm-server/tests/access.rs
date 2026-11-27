//! Access control entries set from the console: the `add_ace` family and the
//! rules that `is_ace_allowed` applies to them.

mod common;

use common::Fixture;
use sixm_core::Host;

/// Whether `principal` may use `object`, as the server decides it.
fn allowed(fx: &Fixture, principal: &str, object: &str) -> bool {
    fx.manager()
        .server()
        .expect("the resource manager is a server host")
        .is_ace_allowed(principal, object)
}

#[test]
fn nothing_is_allowed_until_an_entry_grants_it() {
    let fx = Fixture::new();
    assert!(!allowed(&fx, "player.1", "command.quit"));
    assert!(!allowed(&fx, "group.admin", "command"));
}

#[test]
fn add_ace_grants_the_object_and_its_sub_paths() {
    let fx = Fixture::new();
    fx.server.execute("add_ace group.admin command allow");

    assert!(allowed(&fx, "group.admin", "command"));
    assert!(allowed(&fx, "group.admin", "command.stop"));
    assert!(!allowed(&fx, "group.user", "command.stop"));
}

#[test]
fn a_deny_on_a_more_specific_path_wins_over_an_allow_above_it() {
    let fx = Fixture::new();
    fx.server.execute("add_ace group.admin command allow");
    fx.server.execute("add_ace group.admin command.quit deny");

    assert!(allowed(&fx, "group.admin", "command.start"));
    assert!(!allowed(&fx, "group.admin", "command.quit"));
}

#[test]
fn remove_ace_takes_out_only_the_entry_with_that_permission() {
    let fx = Fixture::new();
    fx.server.execute("add_ace group.admin command allow");
    fx.server.execute("remove_ace group.admin command deny");
    assert!(allowed(&fx, "group.admin", "command"));

    fx.server.execute("remove_ace group.admin command allow");
    assert!(!allowed(&fx, "group.admin", "command"));
}

#[test]
fn add_principal_and_remove_principal_change_inheritance() {
    let fx = Fixture::new();
    fx.server.execute("add_ace group.admin command allow");
    fx.server.execute("add_principal player.4 group.admin");
    assert!(allowed(&fx, "player.4", "command.stop"));

    fx.server.execute("remove_principal player.4 group.admin");
    assert!(!allowed(&fx, "player.4", "command.stop"));
}

#[test]
fn everyone_inherits_from_builtin_everyone() {
    let fx = Fixture::new();
    fx.server
        .execute("add_ace builtin.everyone command.help allow");

    assert!(allowed(&fx, "player.8", "command.help"));
    assert!(allowed(&fx, "group.anything", "command.help"));
    assert!(!allowed(&fx, "player.8", "command.quit"));
}

#[test]
fn names_are_case_insensitive_in_commands_and_checks() {
    let fx = Fixture::new();
    fx.server.execute("ADD_ACE Group.Admin Command ALLOW");

    assert!(allowed(&fx, "group.admin", "command.Stop"));
    fx.server.execute("remove_ace GROUP.ADMIN command allow");
    assert!(!allowed(&fx, "group.admin", "command"));
}

#[test]
fn malformed_access_commands_change_nothing() {
    let fx = Fixture::new();
    fx.server.execute("add_ace group.admin command");
    fx.server.execute("add_ace group.admin command maybe");
    fx.server.execute("add_ace group.admin command allow extra");
    fx.server.execute("add_ace \"\" command allow");
    fx.server.execute("add_principal player.1");
    fx.server.execute("test_ace group.admin");
    fx.server.execute("remove_ace group.admin command allow");

    assert!(!allowed(&fx, "group.admin", "command"));
    assert!(!allowed(&fx, "player.1", "command"));
}

#[test]
fn list_and_test_commands_run_without_changing_entries() {
    let fx = Fixture::new();
    fx.server.execute("list_aces");
    fx.server.execute("list_principals");
    fx.server.execute("add_ace group.admin command allow");
    fx.server.execute("add_principal player.2 group.admin");
    fx.server.execute("list_aces");
    fx.server.execute("list_principals");
    fx.server.execute("test_ace player.2 command.stop");
    fx.server.execute("test_ace player.3 command.stop");

    assert!(allowed(&fx, "player.2", "command.stop"));
    assert!(!allowed(&fx, "player.3", "command.stop"));
}

#[test]
fn a_built_in_command_runs_even_when_a_convar_has_its_name() {
    let fx = Fixture::new();
    fx.server.execute("set add_ace some_value");
    fx.server.execute("add_ace group.admin command allow");

    assert!(allowed(&fx, "group.admin", "command"));
    assert_eq!(
        fx.manager().convar("add_ace").as_deref(),
        Some("some_value")
    );
}
