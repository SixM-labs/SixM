//! The SQL bridge is available only on the server and degrades on hosts without SQL.

mod common;

use common::{first, new_client_host, new_host, start};
use sixm_core::Value;

#[test]
fn server_host_defaults_report_unavailable() {
    let host = new_host();
    let runtime = start(
        &host,
        "sql_server",
        &["check"],
        &[(
            "main.js",
            r"
            exports('check', () => {
                if (SixmSqlIsReady()) throw new Error('database unexpectedly ready');
                const cb = () => { throw new Error('unexpected callback'); };
                const attempts = [
                    () => SixmSqlExecute('query', 'SELECT 1', undefined, cb),
                    () => SixmSqlPrepare('SELECT 1', [], cb),
                    () => SixmSqlTransaction(['SELECT 1'], null, cb)
                ];
                return attempts.every((run) => {
                    try { run(); } catch (error) { return String(error).includes('SQL is unavailable'); }
                    return false;
                });
            });
            ",
        )],
    );
    assert_eq!(first(&runtime, "check"), Value::Bool(true));
}

#[test]
fn clients_do_not_have_a_sql_bridge() {
    let host = new_client_host(1);
    let runtime = start(
        &host,
        "sql_client",
        &["check"],
        &[(
            "main.js",
            r"
            exports('check', () => [
                'SixmSqlIsReady', 'SixmSqlExecute', 'SixmSqlPrepare', 'SixmSqlTransaction'
            ].every((name) => typeof globalThis[name] === 'undefined'));
            ",
        )],
    );
    assert_eq!(first(&runtime, "check"), Value::Bool(true));
}
