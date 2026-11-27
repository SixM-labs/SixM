mod common;

use common::{call, first, new_client_host, new_host, start, text};
use sixm_core::{EntityInfo, EntityKind, Host, PlayerInfo, Value};

#[test]
fn state_bags_proxy_and_change_handlers_work_across_resources() {
    let host = new_host();
    host.players.borrow_mut().push((
        1,
        PlayerInfo {
            name: "Bob".to_owned(),
            endpoint: "127.0.0.1:1".to_owned(),
        },
    ));
    host.entities.borrow_mut().insert(
        5,
        EntityInfo {
            kind: EntityKind::Vehicle,
            model: 1,
            position: [0.0; 3],
            rotation: [0.0; 3],
            owner: None,
        },
    );
    let mut seen_init = false;
    let a = start(
        &host,
        "a",
        &["run"],
        &[(
            "index.js",
            r#"
            const log = [];
            const cookie = AddStateBagChangeHandler("mode", "global", (bag, key, val, reserved, rep) => {
                log.push(`${bag}:${key}:${val}:${reserved}:${rep}`);
            });
            exports("run", () => {
                GlobalState.mode = "race";
                GlobalState.set("secret", 99, false);
                const hasMode = "mode" in GlobalState;
                delete GlobalState.mode;
                const afterDel = GlobalState.mode === undefined;
                RemoveStateBagChangeHandler(cookie);
                GlobalState.mode = "freeroam";
                Entity(5).state.plate = "SIXM";
                Player(1).state.score = 12;
                return [hasMode, afterDel, GlobalState.secret, Entity(5).state.plate, Player(1).state.score, log.join("|")];
            });
            "#,
        )],
    );
    let _ = &mut seen_init;

    assert_eq!(
        call(&a, "run", &[]),
        vec![Value::Array(vec![
            Value::Bool(true),
            Value::Bool(true),
            Value::Int(99),
            Value::from("SIXM"),
            Value::Int(12),
            Value::from("global:mode:race:0:true|global:mode:null:0:true"),
        ])]
    );
    assert_eq!(
        host.state_get("global", "mode"),
        Some(Value::from("freeroam"))
    );

    // Client state bag writes must fail unless replicated == false
    let cli_host = new_client_host(4);
    cli_host.players.borrow_mut().push((
        4,
        PlayerInfo {
            name: "Cli".to_owned(),
            endpoint: "127.0.0.1:4".to_owned(),
        },
    ));
    let cli = start(
        &cli_host,
        "c",
        &["check"],
        &[(
            "index.js",
            r#"
            exports("check", () => {
                let threw = false;
                try { GlobalState.fn = () => {}; } catch (e) { threw = true; }
                LocalPlayer.state.set("localOnly", "yes", false);
                return [threw, LocalPlayer.state.localOnly];
            });
            "#,
        )],
    );
    assert_eq!(
        call(&cli, "check", &[]),
        vec![Value::Array(vec![Value::Bool(true), Value::from("yes")])]
    );
}

#[test]
fn kvp_operations_and_persistence_work() {
    let dir = tempfile::tempdir().expect("tempdir");
    let host1 = new_host();
    host1.set_kvp_dir(dir.path().to_path_buf());
    let rt1 = start(
        &host1,
        "kvp_res",
        &["write"],
        &[(
            "index.js",
            r#"
            exports("write", () => {
                SetResourceKvp("user:1", "alice");
                SetResourceKvpInt("user:count", 42);
                SetResourceKvpFloat("user:ratio", 2.5);
                SetResourceKvpNoSync("temp", "gone");
                DeleteResourceKvp("temp");
                const h = StartFindKvp("user:");
                const keys = [];
                while (true) {
                    const k = FindKvp(h);
                    if (k === null) break;
                    keys.push(k);
                }
                EndFindKvp(h);
                FlushResourceKvp();
                return [
                    GetResourceKvpString("user:1"),
                    GetResourceKvpInt("user:count"),
                    GetResourceKvpFloat("user:ratio"),
                    keys.join(",")
                ];
            });
            "#,
        )],
    );
    assert_eq!(
        call(&rt1, "write", &[]),
        vec![Value::Array(vec![
            Value::from("alice"),
            Value::Int(42),
            Value::Float(2.5),
            Value::from("user:1,user:count,user:ratio"),
        ])]
    );
    host1.stop(&rt1);
    drop(host1);

    let host2 = new_host();
    host2.set_kvp_dir(dir.path().to_path_buf());
    let rt2 = start(
        &host2,
        "kvp_res",
        &["read"],
        &[(
            "index.js",
            r#"exports("read", () => GetResourceKvpString("user:1"));"#,
        )],
    );
    assert_eq!(text(&rt2, "read"), "alice");
}

#[test]
fn resource_files_and_metadata_natives_work() {
    let host = new_host();
    host.metadata.borrow_mut().insert(
        "res".to_owned(),
        vec![
            ("author".to_owned(), "SixM".to_owned()),
            ("author".to_owned(), "Team".to_owned()),
        ],
    );
    let rt = start(
        &host,
        "res",
        &["check"],
        &[
            ("index.js", r#"
                exports("check", () => {
                    SaveResourceFile("res", "out.txt", "saved!");
                    return [
                        GetCurrentResourceName(),
                        LoadResourceFile("res", "data.txt"),
                        LoadResourceFile("res", "out.txt"),
                        GetNumResourceMetadata("res", "author"),
                        GetResourceMetadata("res", "author", 1),
                        GetResourceState("res"),
                        IsDuplicityVersion()
                    ];
                });
            "#),
            ("data.txt", "from-disk"),
        ],
    );

    assert_eq!(
        call(&rt, "check", &[]),
        vec![Value::Array(vec![
            Value::from("res"),
            Value::from("from-disk"),
            Value::from("saved!"),
            Value::Int(2),
            Value::from("Team"),
            Value::from("started"),
            Value::Bool(true),
        ])]
    );
    let _ = first;
}
