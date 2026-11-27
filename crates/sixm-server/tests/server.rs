//! Drives the server API against resource trees written to a temporary folder.

mod common;

use std::time::Instant;

use common::{Fixture, manifest, write_file};
use sixm_core::{Host, ScriptError, Value};
use sixm_server::ManagerError;

/// A `watch` resource that records lifecycle events into `LOG`, exposed as `log()`.
fn add_watch(fx: &Fixture) {
    let script = r"
        LOG = {}
        local function note(entry) LOG[#LOG + 1] = entry end
        AddEventHandler('onResourceStarting', function(name) note('starting:' .. name) end)
        AddEventHandler('onResourceStart', function(name) note('start:' .. name) end)
        AddEventHandler('onResourceStop', function(name) note('stop:' .. name) end)
        function log() return table.concat(LOG, ' ') end
    ";
    fx.add_resource(
        "watch",
        &manifest(&["server.lua"], &["log"], &[]),
        &[("server.lua", script)],
    );
}

/// `base` and `app`, where `app` depends on `base`.
fn add_base_and_app(fx: &Fixture) {
    fx.add_resource(
        "base",
        &manifest(&["server.lua"], &[], &[]),
        &[("server.lua", "BASE = true")],
    );
    fx.add_resource(
        "app",
        &manifest(&["server.lua"], &[], &["base"]),
        &[("server.lua", "APP = true")],
    );
}

fn watch_log(fx: &Fixture) -> String {
    fx.text("watch", "log")
}

#[test]
fn set_stores_convars_case_insensitively_with_joined_values() {
    let fx = Fixture::new();
    fx.server.execute("set Greeting   hello   big   world");
    assert_eq!(
        fx.manager().convar("greeting").as_deref(),
        Some("hello big world")
    );

    fx.server
        .execute(r#"sets quoted "two  spaces # not a comment""#);
    assert_eq!(
        fx.manager().convar("quoted").as_deref(),
        Some("two  spaces # not a comment")
    );

    fx.server.execute("setr replicated yes");
    assert_eq!(
        fx.manager().get_convar("REPLICATED").as_deref(),
        Some("yes")
    );
}

#[test]
fn convar_line_alone_leaves_the_value_unchanged() {
    let fx = Fixture::new();
    fx.server.execute("set probe kept");
    fx.server.execute("probe");
    assert_eq!(fx.manager().convar("probe").as_deref(), Some("kept"));
}

#[test]
fn exec_runs_each_line_and_skips_comments() {
    let fx = Fixture::new();
    let cfg = fx.root().join("server.cfg");
    write_file(
        &cfg,
        "# a comment line\n\nset from_file one // trailing comment\nset quoted_line \"a b\"\n",
    );
    fx.server.execute(&format!("exec \"{}\"", cfg.display()));
    assert_eq!(fx.manager().convar("from_file").as_deref(), Some("one"));
    assert_eq!(fx.manager().convar("quoted_line").as_deref(), Some("a b"));
}

#[test]
fn exec_that_includes_itself_terminates() {
    let fx = Fixture::new();
    let cfg = fx.root().join("loop.cfg");
    write_file(
        &cfg,
        &format!("set reached yes\nexec \"{}\"\n", cfg.display()),
    );
    fx.server.execute(&format!("exec \"{}\"", cfg.display()));
    assert_eq!(fx.manager().convar("reached").as_deref(), Some("yes"));
}

#[test]
fn script_commands_receive_source_arguments_and_raw_line() {
    let fx = Fixture::new();
    let script = r"
        RegisterCommand('greet', function(source, args, raw)
            LAST = ('%s|%d|%s|%s'):format(source, #args, table.concat(args, ','), raw)
        end)
        function last() return LAST or 'none' end
    ";
    fx.add_resource(
        "cmds",
        &manifest(&["server.lua"], &["last"], &[]),
        &[("server.lua", script)],
    );
    fx.manager().start("cmds").expect("start cmds");

    fx.server.execute(r#"GREET a "b c""#);
    assert_eq!(fx.text("cmds", "last"), r#"0|2|a,b c|GREET a "b c""#);
}

#[test]
fn execute_command_runs_after_the_current_call() {
    let fx = Fixture::new();
    let script = r"
        SEEN = 'not run'
        RegisterCommand('probe', function()
            ExecuteCommand('set deferred_value yes')
            SEEN = GetConvar('deferred_value', 'unset')
        end)
        function seen() return SEEN end
    ";
    fx.add_resource(
        "deferrer",
        &manifest(&["server.lua"], &["seen"], &[]),
        &[("server.lua", script)],
    );
    fx.manager().start("deferrer").expect("start deferrer");

    fx.server.execute("probe");
    assert_eq!(fx.text("deferrer", "seen"), "unset");
    assert_eq!(
        fx.manager().convar("deferred_value").as_deref(),
        Some("yes")
    );
}

#[test]
fn dependencies_start_first_and_events_are_observed() {
    let fx = Fixture::new();
    add_watch(&fx);
    add_base_and_app(&fx);
    fx.server.execute("start watch");
    fx.server.execute("start app");

    assert_eq!(
        watch_log(&fx),
        "start:watch starting:base start:base starting:app start:app"
    );
    assert_eq!(fx.manager().running_resources().len(), 3);
}

#[test]
fn stop_stops_dependents_first_and_fires_stop_events() {
    let fx = Fixture::new();
    add_watch(&fx);
    add_base_and_app(&fx);
    fx.manager().start("app").expect("start app");
    fx.manager().start("watch").expect("start watch");

    fx.manager().stop("base").expect("stop base");

    assert!(
        watch_log(&fx).ends_with("stop:app stop:base"),
        "{}",
        watch_log(&fx)
    );
    assert!(!fx.manager().is_running("base"));
    assert!(!fx.manager().is_running("app"));
    assert!(fx.manager().is_running("watch"));
}

#[test]
fn stopping_a_resource_that_is_not_running_is_an_error() {
    let fx = Fixture::new();
    add_base_and_app(&fx);
    assert!(matches!(
        fx.manager().stop("base"),
        Err(ManagerError::NotRunning(name)) if name == "base"
    ));
}

#[test]
fn restart_stops_and_starts_dependents_again() {
    let fx = Fixture::new();
    add_watch(&fx);
    add_base_and_app(&fx);
    fx.manager().start("app").expect("start app");
    fx.manager().start("watch").expect("start watch");

    fx.manager().restart("base").expect("restart base");

    assert!(
        watch_log(&fx)
            .ends_with("stop:app stop:base starting:base start:base starting:app start:app"),
        "{}",
        watch_log(&fx)
    );
    assert!(fx.manager().is_running("app"));
}

#[test]
fn restart_of_a_stopped_resource_starts_it() {
    let fx = Fixture::new();
    add_base_and_app(&fx);
    fx.manager().restart("base").expect("restart base");
    assert!(fx.manager().is_running("base"));
}

#[test]
fn ensure_starts_then_restarts() {
    let fx = Fixture::new();
    add_watch(&fx);
    add_base_and_app(&fx);
    fx.manager().start("watch").expect("start watch");

    fx.manager().ensure("app").expect("ensure app");
    assert!(fx.manager().is_running("base"));
    assert!(fx.manager().is_running("app"));

    fx.manager().ensure("app").expect("ensure app again");
    assert!(
        watch_log(&fx).ends_with("stop:app starting:app start:app"),
        "{}",
        watch_log(&fx)
    );
    assert!(fx.manager().is_running("base"));
}

#[test]
fn onresourcestarting_can_cancel_a_start() {
    let fx = Fixture::new();
    let guard = r"
        AddEventHandler('onResourceStarting', function(name)
            if name == 'blocked' then CancelEvent() end
        end)
    ";
    fx.add_resource(
        "guard",
        &manifest(&["server.lua"], &[], &[]),
        &[("server.lua", guard)],
    );
    fx.add_resource(
        "blocked",
        &manifest(&["server.lua"], &[], &[]),
        &[("server.lua", "BLOCKED_RAN = true")],
    );
    fx.manager().start("guard").expect("start guard");

    assert!(matches!(
        fx.manager().start("blocked"),
        Err(ManagerError::Canceled(name)) if name == "blocked"
    ));
    assert!(!fx.manager().is_running("blocked"));
}

#[test]
fn failing_script_does_not_stop_the_others_from_loading() {
    let fx = Fixture::new();
    fx.add_resource(
        "partial",
        &manifest(&["broken.lua", "ok.lua", "export.lua"], &["ok_loaded"], &[]),
        &[
            ("broken.lua", "this is not lua ("),
            ("ok.lua", "OK_LOADED = true"),
            (
                "export.lua",
                "function ok_loaded() return tostring(OK_LOADED) end",
            ),
        ],
    );
    fx.manager().start("partial").expect("start partial");
    assert_eq!(fx.text("partial", "ok_loaded"), "true");
}

#[test]
fn function_references_of_a_stopped_resource_are_stale() {
    let fx = Fixture::new();
    fx.add_resource(
        "maker",
        &manifest(&["server.lua"], &["make"], &[]),
        &[(
            "server.lua",
            "function make() return function() return 42 end end",
        )],
    );
    fx.manager().start("maker").expect("start maker");

    let values = fx
        .manager()
        .call_export("maker", "make", &[], None)
        .expect("make");
    let Some(Value::FuncRef(func)) = values.into_iter().next() else {
        panic!("export did not return a function reference");
    };
    assert_eq!(
        fx.manager()
            .call_ref(&func, &[])
            .expect("call while running"),
        vec![Value::Int(42)]
    );

    fx.manager().stop("maker").expect("stop maker");
    assert!(matches!(
        fx.manager().call_ref(&func, &[]),
        Err(ScriptError::StaleRef(_))
    ));
    assert!(!fx.manager().retain_ref(&func));
}

#[test]
fn refresh_picks_up_a_new_resource() {
    let fx = Fixture::new();
    fx.add_resource(
        "first",
        &manifest(&["server.lua"], &[], &[]),
        &[("server.lua", "")],
    );
    fx.manager().start("first").expect("start first");

    fx.add_resource(
        "second",
        &manifest(&["server.lua"], &[], &[]),
        &[("server.lua", "")],
    );
    assert!(matches!(
        fx.manager().start("second"),
        Err(ManagerError::Dependency(_))
    ));

    fx.server.execute("refresh");
    fx.manager()
        .start("second")
        .expect("start second after refresh");
    assert!(fx.manager().is_running("second"));
}

#[test]
fn unknown_and_cyclic_resources_are_errors_not_panics() {
    let fx = Fixture::new();
    fx.add_resource(
        "loop_a",
        &manifest(&["server.lua"], &[], &["loop_b"]),
        &[("server.lua", "")],
    );
    fx.add_resource(
        "loop_b",
        &manifest(&["server.lua"], &[], &["loop_a"]),
        &[("server.lua", "")],
    );

    assert!(matches!(
        fx.manager().start("missing"),
        Err(ManagerError::Dependency(_))
    ));
    assert!(matches!(
        fx.manager().start("loop_a"),
        Err(ManagerError::Dependency(_))
    ));
    assert_eq!(fx.manager().running_resources().len(), 0);
}

#[test]
fn frames_resume_scripts_that_wait() {
    let fx = Fixture::new();
    let script = r"
        DONE = false
        CreateThread(function() DONE = true end)
        function done() return tostring(DONE) end
    ";
    fx.add_resource(
        "timer",
        &manifest(&["server.lua"], &["done"], &[]),
        &[("server.lua", script)],
    );
    fx.manager().start("timer").expect("start timer");
    assert_eq!(fx.text("timer", "done"), "false");

    fx.server.tick(Instant::now());
    assert_eq!(fx.text("timer", "done"), "true");
}

#[test]
fn quit_requests_shutdown() {
    let fx = Fixture::new();
    assert!(!fx.manager().quit_requested());
    fx.server.execute("quit maintenance");
    assert!(fx.manager().quit_requested());
}
