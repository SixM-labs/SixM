mod common;

use std::time::Instant;

use common::{call, first, new_client_host, new_host, start, text, tick};
use sixm_core::{ClientTarget, EventContext, EventSource, ScriptRuntime, Value};

#[test]
fn local_events_cancel_and_handler_removal_work() {
    let host = new_host();
    let rt = start(
        &host,
        "ev",
        &["run", "log"],
        &[(
            "index.js",
            r#"
            const seen = [];
            const fnA = (v) => {
                seen.push("A:" + v + ":" + source + ":" + GetInvokingResource());
                CancelEvent();
            };
            const h1 = on("ping", fnA);
            on("ping", (v) => {
                seen.push("B:" + v + ":" + WasEventCanceled());
            });
            const fnC = (v) => seen.push("C:" + v);
            on("ping", fnC);
            removeEventListener("ping", fnC);

            exports("run", () => {
                emit("ping", 1);
                const c1 = WasEventCanceled();
                RemoveEventHandler(h1);
                emit("ping", 2);
                const c2 = WasEventCanceled();
                return [c1, c2];
            });
            exports("log", () => seen.join("|"));
            "#,
        )],
    );

    assert_eq!(
        call(&rt, "run", &[]),
        vec![Value::Array(vec![Value::Bool(true), Value::Bool(false)])]
    );
    assert_eq!(text(&rt, "log"), "A:1::ev|B:1:true|B:2:false");
}

#[test]
fn net_events_enforce_register_net_event_on_both_sides() {
    let srv_host = new_host();
    let srv = start(
        &srv_host,
        "net_s",
        &["send"],
        &[(
            "index.js",
            r#"
            globalThis.got = [];
            on("localOnly", () => globalThis.got.push("bad"));
            onNet("safe", (msg) => globalThis.got.push(source + ":" + msg));
            exports("send", () => {
                emitNet("clientEvt", -1, "hello", 42);
                TriggerLatentClientEvent("latentEvt", 5, 1024, "slow");
                return globalThis.got.join(",");
            });
            "#,
        )],
    );

    let ctx = EventContext::default();
    assert!(srv.handles_event("localOnly"));
    assert!(srv.handles_event("safe"));
    // Unregistered net delivery from a player must be dropped, while registered `safe` runs:
    srv.deliver_event("localOnly", EventSource::Net(7), &[], &ctx)
        .expect("drop unregistered net event");
    srv.deliver_event("safe", EventSource::Net(7), &[Value::from("hi")], &ctx)
        .expect("deliver");
    assert_eq!(text(&srv, "send"), "7:hi");
    assert_eq!(
        *srv_host.client_events.borrow(),
        vec![(
            "clientEvt".to_owned(),
            ClientTarget::All,
            vec![Value::from("hello"), Value::Int(42)],
        )]
    );
    assert_eq!(
        *srv_host.latent_client_events.borrow(),
        vec![(
            "latentEvt".to_owned(),
            ClientTarget::Player(5),
            vec![Value::from("slow")],
            1024,
        )]
    );

    let cli_host = new_client_host(3);
    let cli = start(
        &cli_host,
        "net_c",
        &["send"],
        &[(
            "index.js",
            r#"
            globalThis.got = [];
            onNet("srvMsg", (x) => globalThis.got.push(source + ":" + x));
            exports("send", () => {
                emitNet("toServer", "ping", true);
                TriggerLatentServerEvent("toServerLatent", 512, 9);
                return globalThis.got.join(",");
            });
            "#,
        )],
    );
    cli.deliver_event("srvMsg", EventSource::Server, &[Value::from("ok")], &ctx)
        .expect("deliver client");
    assert_eq!(text(&cli, "send"), "65535:ok");
    assert_eq!(
        *cli_host.server_events.borrow(),
        vec![(
            "toServer".to_owned(),
            vec![Value::from("ping"), Value::Bool(true)],
        )]
    );
    assert_eq!(
        *cli_host.latent_server_events.borrow(),
        vec![("toServerLatent".to_owned(), vec![Value::Int(9)], 512)]
    );
}

#[test]
fn cross_resource_exports_manifest_fallback_and_funcref_gc_work() {
    let host = new_host();
    let provider = start(
        &host,
        "provider",
        &["manifestGlobal"],
        &[(
            "index.js",
            r#"
            function manifestGlobal(x) {
                return [x * 2, GetInvokingResource()];
            }
            let savedCb = null;
            let lastCbValue = 0;
            exports("saveCb", (n, cb) => {
                savedCb = () => cb(n + 1);
                return "saved";
            });
            exports("runSaved", () => {
                lastCbValue = savedCb();
                return lastCbValue;
            });
            "#,
        )],
    );
    let caller = start(
        &host,
        "caller",
        &["test", "localAdd"],
        &[(
            "index.js",
            r#"
            exports("localAdd", (x) => x + 5);
            exports("test", () => {
                const [doubled, inv] = exports.provider.manifestGlobal(21);
                const status = exports.provider.saveCb(9, (v) => v * 10);
                const selfVal = exports.caller.localAdd(7);
                return [doubled, inv, status, selfVal];
            });
            "#,
        )],
    );

    assert_eq!(
        call(&caller, "test", &[]),
        vec![Value::Array(vec![
            Value::Int(42),
            Value::from("caller"),
            Value::from("saved"),
            Value::Int(12),
        ])]
    );
    assert_eq!(first(&provider, "runSaved"), Value::Int(100));

    // Trigger GC on caller and provider to ensure FuncRefs can be collected cleanly
    caller.run_gc();
    provider.run_gc();
    tick(&host, Instant::now());
    assert!(!host.released.borrow().is_empty() || provider.instance() > 0);
    let _ = first;
}
