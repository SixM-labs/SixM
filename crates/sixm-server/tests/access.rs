//! Access control entries set from the console: the `add_ace` family and the
//! rules that `is_ace_allowed` applies to them. Also the rights that a script's
//! `ExecuteCommand` lines run with, which are those of `resource.<name>`.

mod common;

use common::{Fixture, manifest, write_file};
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

/// A resource `name` whose console command `command` runs `line` with
/// `ExecuteCommand`. `line` is written into a Lua long string.
fn add_relay(fx: &Fixture, name: &str, command: &str, line: &str) {
    let script =
        format!("RegisterCommand('{command}', function() ExecuteCommand([==[{line}]==]) end)\n");
    fx.add_resource(
        name,
        &manifest(&["server.lua"], &[], &[]),
        &[("server.lua", &script)],
    );
}

#[test]
fn a_resource_cannot_stop_a_resource_without_an_ace() {
    let fx = Fixture::new();
    fx.add_resource(
        "other",
        &manifest(&["server.lua"], &[], &[]),
        &[("server.lua", "OTHER = true")],
    );
    add_relay(&fx, "driver", "kill", "stop other");
    fx.server.execute("start other");
    fx.server.execute("start driver");

    fx.server.execute("kill");
    assert!(fx.manager().is_running("other"));

    fx.server
        .execute("add_ace resource.driver command.stop allow");
    fx.server.execute("kill");
    assert!(!fx.manager().is_running("other"));
}

#[test]
fn a_resource_cannot_read_or_set_a_convar_by_its_name() {
    let fx = Fixture::new();
    fx.server.execute("set sv_hostname original");
    add_relay(&fx, "driver", "rename", "sv_hostname x");
    fx.server.execute("start driver");

    fx.server.execute("rename");
    assert_eq!(
        fx.manager().convar("sv_hostname").as_deref(),
        Some("original")
    );

    fx.server
        .execute("add_ace resource.driver command.set allow");
    fx.server.execute("rename");
    assert_eq!(
        fx.manager().convar("sv_hostname").as_deref(),
        Some("original")
    );
}

#[test]
fn a_grant_to_builtin_everyone_reaches_resources() {
    let fx = Fixture::new();
    add_relay(&fx, "driver", "store", "set from_everyone yes");
    fx.server.execute("start driver");

    fx.server.execute("store");
    assert_eq!(fx.manager().convar("from_everyone"), None);

    fx.server
        .execute("add_ace builtin.everyone command.set allow");
    fx.server.execute("store");
    assert_eq!(fx.manager().convar("from_everyone").as_deref(), Some("yes"));
}

#[test]
fn an_unrestricted_script_command_runs_from_execute_command_with_source_zero() {
    let fx = Fixture::new();
    let script = r"
        RegisterCommand('ping', function(source, args)
            LAST = ('%d|%s'):format(source, table.concat(args, ','))
        end)
        RegisterCommand('relay', function() ExecuteCommand('ping a b') end)
        function last() return LAST or 'none' end
    ";
    fx.add_resource(
        "pinger",
        &manifest(&["server.lua"], &["last"], &[]),
        &[("server.lua", script)],
    );
    fx.server.execute("start pinger");

    fx.server.execute("relay");
    assert_eq!(fx.text("pinger", "last"), "0|a,b");
}

#[test]
fn a_restricted_script_command_needs_the_ace_from_a_resource() {
    let fx = Fixture::new();
    let script = r"
        RegisterCommand('secret', function() SEEN = 'yes' end, true)
        RegisterCommand('relay', function() ExecuteCommand('secret') end)
        function seen() return SEEN or 'no' end
    ";
    fx.add_resource(
        "vault",
        &manifest(&["server.lua"], &["seen"], &[]),
        &[("server.lua", script)],
    );
    fx.server.execute("start vault");

    fx.server.execute("relay");
    assert_eq!(fx.text("vault", "seen"), "no");

    fx.server
        .execute("add_ace resource.vault command.secret allow");
    fx.server.execute("relay");
    assert_eq!(fx.text("vault", "seen"), "yes");
}

#[test]
fn exec_from_a_resource_keeps_the_resources_rights() {
    let fx = Fixture::new();
    let cfg = fx.root().join("driver.cfg");
    write_file(&cfg, "set from_file yes\nquit\n");
    add_relay(
        &fx,
        "loader",
        "load",
        &format!("exec \"{}\"", cfg.display()),
    );
    fx.server.execute("start loader");

    fx.server.execute("load");
    assert_eq!(fx.manager().convar("from_file"), None);

    fx.server
        .execute("add_ace resource.loader command.exec allow");
    fx.server
        .execute("add_ace resource.loader command.set allow");
    fx.server.execute("load");
    assert_eq!(fx.manager().convar("from_file").as_deref(), Some("yes"));
    assert!(!fx.manager().quit_requested());
}
