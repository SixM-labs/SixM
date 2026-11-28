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
            "main.lua",
            r"
            exports('check', function()
                assert(SixmSqlIsReady() == false)
                local cb = function() error('unexpected callback') end
                local ok, err = pcall(SixmSqlExecute, 'query', 'SELECT 1', nil, cb)
                assert(not ok and tostring(err):find('SQL is unavailable', 1, true))
                assert(not pcall(SixmSqlPrepare, 'SELECT 1', {}, cb))
                assert(not pcall(SixmSqlTransaction, { 'SELECT 1' }, nil, cb))
                return true
            end)
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
            "main.lua",
            "exports('check', function()\n\
               return SixmSqlIsReady == nil and SixmSqlExecute == nil and\n\
                      SixmSqlPrepare == nil and SixmSqlTransaction == nil\n\
             end)",
        )],
    );
    assert_eq!(first(&runtime, "check"), Value::Bool(true));
}
